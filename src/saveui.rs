//! The game's own save screens, widened from 5 slots to 10; 存檔 / 讀檔 added to
//! the battle 功能 menu; slot 10 kept as an automatic save at the end of every
//! player phase.
//!
//! MAIN.EXE lists its saves (`MSAVEn.R3S` + `ESAVEn.R3S`, n = 0-based slot) in
//! two functions — load list at 168b:ae4c, save list at 168b:aaa0 (segments as
//! loaded at 0995; entered through 168b:b2c2 / 168b:b2ce with AX=100h = hard
//! disk). Each builds five 0x40-byte rows on its stack frame, a table of row
//! pointers at DS:C940 and (load list) a byte per row saying whether it holds a
//! save, then hands them to the menu routine 168b:29f8 with a row count of 5. The
//! chosen 0-based slot goes to 168b:a862 (load) / 168b:a3ac (save), which put the
//! slot digit into the file name, so slots 5-9 need no change there.
//!
//! The game's save files only hold the campaign between battles. So the runtime
//! keeps a whole-machine state next to a save (`savestates/slotN.state`, N =
//! 1-10) plus the save's header at that time (`slotN.key`): loading a slot whose
//! MSAVE header still matches restores the machine — mid-battle or wherever it
//! was saved. A slot whose files were written some other way (older saves, the
//! game's original save) loads the game's own way, from the title screen.
//!
//! The patches (applied in memory when MAIN.EXE is loaded; the file on disk is
//! untouched, and nothing is patched unless every original byte matches):
//! - save/load lists: frames grow so ten rows fit (rows, row flags and the
//!   pointer table move into the frame; DS:C940 + 44 bytes would run into the
//!   much-used DS:C95E; SS = DS in this program, so a stack address works as the
//!   menu's DS pointer), row count 5 -> 10, saved background 90h -> 140h rows;
//! - battle 功能 menu (in the player phase, 2687:3682): its item table
//!   (DS:2D6E, ten far pointers, DEBUG last and hidden) is rebuilt on the stack
//!   with 存檔 and 讀檔 before 遊戲結束, two more visible rows, and a hook maps
//!   the chosen item back to the game's own numbering;
//! - `FE 38 nn` hooks (`hook` below) in place of: the far calls that load or save
//!   the chosen slot, the call after the player phase of a battle round (2687:2f56
//!   runs a round: 2687:3682 = our phase, 2687:30d6 = the rest), the battle menu
//!   item, and the lists' title-box / background-restore calls (each screen
//!   redraws only its scene view after a window closes; the 10-row lists reach
//!   over the frames, so the runtime keeps the picture and puts it back).
//!
//! Small routines and data live in the BIOS segment F000 (see `LAYOUT`).

use crate::cpu::{CF, CS, SS, ZF};
use crate::machine::{Machine, R, BIOS_SEG};

pub const SLOTS: u8 = 10;
pub const AUTO_SLOT: u8 = SLOTS - 1;

const HOOK_LOAD: u8 = 0xa0;
const HOOK_SAVE: u8 = 0xa1;
const HOOK_PHASE: u8 = 0xa2;
const HOOK_MENU: u8 = 0xa3;
const HOOK_SAVED: u8 = 0xa4;
const HOOK_BG_SAVE: u8 = 0xa6;
const HOOK_BG_RESTORE: u8 = 0xa7;
/// Bytes per plane of the visible 640x480 picture.
const SCREEN_PLANE: usize = 80 * 480;
/// Segments relative to MAIN.EXE's load segment.
const SEG_UI: u16 = 0x168b - 0x995;
const SEG_BATTLE: u16 = 0x2687 - 0x995;
const SEG_DATA: u16 = 0x3a12 - 0x995;
/// Battle state in the data segment: +1 over (won/lost/quit), +2 round
/// (0-based), +6 battle name (Big5, NUL-terminated).
const BATTLE: u16 = 0xcf6a;
/// The battle 功能 menu's item table: ten far pointers, then a null one.
const MENU_TABLE: u16 = 0x2d6e;

// ---- F000 layout ----
/// `lea ax,[bp-N]; mov [bp-10h],ax; retf`: where the save/load lists put their row tables.
const CAVE_LOAD: u16 = 0x0900;
const CAVE_SAVE: u16 = 0x0908;
// Variables in RAM: the BIOS data area's "intra-application communication
// area" 0000:04F0-04FF (F000 is ROM to the CPU).
/// 1 while the save/load list runs from the battle menu.
const FLAG_BATTLE: u16 = 0x04f0;
/// Slot being saved by the game's own routine (for HOOK_SAVED).
const SAVING_SLOT: u16 = 0x04f1;
/// Where CAVE_MENU_TABLE last put the menu block (DS offset).
const MENU_ADDR: u16 = 0x04f2;
/// Back from the list to the battle menu: clear FLAG_BATTLE, jmp 2687:3b07.
const CAVE_MENU_RET: u16 = 0x0918;
/// Copies the battle menu block to bp-72h (replaces `mov ax,dx; lea bx,[2d6e]`).
const CAVE_MENU_TABLE: u16 = 0x0930;
/// Battle menu block: 12 far pointers + null, then the texts at these offsets.
const MENU_TEMPLATE: u16 = 0x0980;
const T_SAVE: u16 = 52;
const T_LOAD: u16 = 58;
const T_OLD_SAVE: u16 = 64;
const TEMPLATE_LEN: u16 = 0x72;
/// After the "load it from the title" message: drop its 3 words, AX=-1, back into
/// the load list's epilogue (168b:b22b).
const CAVE_MSG_RET: u16 = 0x0a00;
/// After the game wrote a save: hook, then back into the save list (168b:ae46).
const CAVE_SAVED: u16 = 0x0a10;

/// (offset in the load image, original bytes, patched bytes). Generated from
/// MAIN.EXE and checked against it; the original bytes are the file's (before
/// relocation).
const PATCHES: &[(usize, &[u8], &[u8])] = &[
    // load: frame 0x1ae -> 0x32e
    (0x17dac, &[0xc8, 0xae, 0x01, 0x00], &[0xc8, 0x2e, 0x03, 0x00]),
    // load: saved bx -0x1b0 -> -0x330
    (0x17de5, &[0x39, 0xb6, 0x50, 0xfe], &[0x39, 0xb6, 0xd0, 0xfc]),
    // load: saved bx -0x1b0 -> -0x330
    (0x18165, &[0x83, 0xbe, 0x50, 0xfe], &[0x83, 0xbe, 0xd0, 0xfc]),
    // load: saved dx -0x1b2 -> -0x332
    (0x17def, &[0x8b, 0x86, 0x4e, 0xfe], &[0x8b, 0x86, 0xce, 0xfc]),
    // load: saved dx -0x1b2 -> -0x332
    (0x17e2d, &[0x8b, 0x96, 0x4e, 0xfe], &[0x8b, 0x96, 0xce, 0xfc]),
    // load: saved dx -0x1b2 -> -0x332
    (0x17e3d, &[0x8b, 0x86, 0x4e, 0xfe], &[0x8b, 0x86, 0xce, 0xfc]),
    // load: saved dx -0x1b2 -> -0x332
    (0x18120, &[0x8b, 0x96, 0x4e, 0xfe], &[0x8b, 0x96, 0xce, 0xfc]),
    // load: saved ax -0x1b4 -> -0x334
    (0x17e67, &[0x89, 0xbe, 0x4c, 0xfe], &[0x89, 0xbe, 0xcc, 0xfc]),
    // load: saved ax -0x1b4 -> -0x334
    (0x18102, &[0x8b, 0xbe, 0x4c, 0xfe], &[0x8b, 0xbe, 0xcc, 0xfc]),
    // load: local -0x1ae -> -0x32e
    (0x18110, &[0x89, 0x86, 0x52, 0xfe], &[0x89, 0x86, 0xd2, 0xfc]),
    // load: local -0x1ae -> -0x32e
    (0x18127, &[0x8b, 0x86, 0x52, 0xfe], &[0x8b, 0x86, 0xd2, 0xfc]),
    // load: 10 rows of text from bp-0x300
    (0x17e5d, &[0x8d, 0x86, 0x54, 0xfe], &[0x8d, 0x86, 0x00, 0xfd]),
    // load: rows end at bp-0x80
    (0x180e9, &[0x8d, 0x46, 0x94], &[0x8d, 0x46, 0x80]),
    // load: row flags at bp-0x7c
    (0x180d1, &[0x88, 0x42, 0xcc], &[0x88, 0x42, 0x84]),
    // load: row flags at bp-0x7c (menu argument)
    (0x18118, &[0x8d, 0x46, 0xcc], &[0x8d, 0x46, 0x84]),
    // load: 10 menu rows
    (0x18114, &[0x6a, 0x05], &[0x6a, 0x0a]),
    // load: saved background 0x90 -> 0x140 high
    (0x17e48, &[0x68, 0x90, 0x00], &[0x68, 0x40, 0x01]),
    // load: row pointer table on the stack (F000:0900)
    (0x17e53, &[0xc7, 0x46, 0xf0, 0x40, 0xc9], &[0x9a, 0x00, 0x09, 0x00, 0xf0]),
    // load: menu reads the table at bp-0x32c
    (0x1812b, &[0x8d, 0x1e, 0x40, 0xc9], &[0x8d, 0x9e, 0xd4, 0xfc]),
    // load: hook before loading slot N
    (0x18186, &[0x9a, 0x62, 0xa8, 0xf6, 0x0c], &[0xfe, 0x38, 0xa0, 0x90, 0x90]),
    // save: frame 0x1a4 -> 0x310
    (0x17a00, &[0xc8, 0xa4, 0x01, 0x00], &[0xc8, 0x10, 0x03, 0x00]),
    // save: saved bx -0x1a6 -> -0x312
    (0x17a93, &[0x81, 0xbe, 0x5a, 0xfe], &[0x81, 0xbe, 0xee, 0xfc]),
    // save: saved bx -0x1a6 -> -0x312
    (0x17ae6, &[0x81, 0xbe, 0x5a, 0xfe], &[0x81, 0xbe, 0xee, 0xfc]),
    // save: saved bx -0x1a6 -> -0x312
    (0x17aff, &[0xff, 0xb6, 0x5a, 0xfe], &[0xff, 0xb6, 0xee, 0xfc]),
    // save: saved bx -0x1a6 -> -0x312
    (0x17d9d, &[0x8b, 0x9e, 0x5a, 0xfe], &[0x8b, 0x9e, 0xee, 0xfc]),
    // save: saved dx -0x1a8 -> -0x314
    (0x17a8f, &[0x89, 0xb6, 0x58, 0xfe], &[0x89, 0xb6, 0xec, 0xfc]),
    // save: saved dx -0x1a8 -> -0x314
    (0x17d43, &[0x8b, 0x96, 0x58, 0xfe], &[0x8b, 0x96, 0xec, 0xfc]),
    // save: saved ax -0x1aa -> -0x316
    (0x17a8b, &[0x89, 0xbe, 0x56, 0xfe], &[0x89, 0xbe, 0xea, 0xfc]),
    // save: saved ax -0x1aa -> -0x316
    (0x17d35, &[0x8b, 0xbe, 0x56, 0xfe], &[0x8b, 0xbe, 0xea, 0xfc]),
    // save: 10 rows of text from bp-0x2e4
    (0x17aa2, &[0x8d, 0x86, 0x5c, 0xfe], &[0x8d, 0x86, 0x1c, 0xfd]),
    // save: 10 menu rows
    (0x17d39, &[0x6a, 0x05], &[0x6a, 0x0a]),
    // save: saved background 0x90 -> 0x140 high
    (0x17a76, &[0x68, 0x90, 0x00], &[0x68, 0x40, 0x01]),
    // save: row pointer table on the stack (F000:0908)
    (0x17a81, &[0xc7, 0x46, 0xf0, 0x40, 0xc9], &[0x9a, 0x08, 0x09, 0x00, 0xf0]),
    // save: menu reads the table at bp-0x310
    (0x17d4c, &[0x8d, 0x1e, 0x40, 0xc9], &[0x8d, 0x9e, 0xf0, 0xfc]),
    // save: hook before saving slot N
    (0x17da1, &[0x9a, 0xac, 0xa3, 0xf6, 0x0c], &[0xfe, 0x38, 0xa1, 0x90, 0x90]),
    // battle: hook after our phase
    (0x1fecc, &[0x9a, 0xd6, 0x30, 0xf2, 0x1c], &[0xfe, 0x38, 0xa2, 0x90, 0x90]),
    // load: hook before drawing the title box
    (0x17e37, &[0x9a, 0x38, 0x23, 0xf6, 0x0c], &[0xfe, 0x38, 0xa6, 0x90, 0x90]),
    // load: hook before restoring the background
    (0x18160, &[0x9a, 0x30, 0xc6, 0xf6, 0x0c], &[0xfe, 0x38, 0xa7, 0x90, 0x90]),
    // save: hook before drawing the title box
    (0x17a69, &[0x9a, 0x38, 0x23, 0xf6, 0x0c], &[0xfe, 0x38, 0xa6, 0x90, 0x90]),
    // save: hook before restoring the background
    (0x17d81, &[0x9a, 0x30, 0xc6, 0xf6, 0x0c], &[0xfe, 0x38, 0xa7, 0x90, 0x90]),
    // battle menu: frame 0x16 -> 0x72 (menu table + texts at bp-0x72)
    (0x205a2, &[0xc8, 0x16, 0x00, 0x00], &[0xc8, 0x72, 0x00, 0x00]),
    // battle menu: two more visible rows
    (0x20883, &[0x83, 0xc6, 0x0b], &[0x83, 0xc6, 0x0d]),
    // battle menu: table with save/load items built on the stack (F000:0930)
    (0x20896, &[0x8b, 0xc2, 0x8d, 0x1e, 0x6e, 0x2d], &[0x9a, 0x30, 0x09, 0x00, 0xf0, 0x90]),
    // battle menu: hook on the chosen item
    (0x208a1, &[0x88, 0x46, 0xff, 0x3c, 0x08], &[0xfe, 0x38, 0xa3, 0x90, 0x90]),
];

fn lin(seg: u16, off: u16) -> u32 {
    ((seg as u32) << 4) + off as u32
}

fn big5(s: &str) -> Vec<u8> {
    encoding_rs::BIG5.encode(s).0.into_owned()
}

/// Patch a freshly loaded program if it is this game's MAIN.EXE (`body` = load
/// image as in the file, `load_seg` = where it was put; relocations already
/// applied in memory). Returns whether it did.
pub fn patch(m: &mut Machine, body: &[u8], load_seg: u16) -> bool {
    if !PATCHES.iter().all(|(o, old, _)| body.get(*o..o + old.len()) == Some(*old)) {
        return false;
    }
    let base = lin(load_seg, 0);
    for (o, _, new) in PATCHES {
        m.mem.write_bytes(base + *o as u32, new);
    }
    let (ui, bat, ds) = (load_seg + SEG_UI, load_seg + SEG_BATTLE, load_seg + SEG_DATA);
    let [ui_lo, ui_hi] = ui.to_le_bytes();
    let [bat_lo, bat_hi] = bat.to_le_bytes();
    let le = |v: u16| v.to_le_bytes();
    m.mem.write_bytes(FLAG_BATTLE as u32, &[0; 4]);
    let mut put = |off: u16, b: &[u8]| m.mem.write_bytes(lin(BIOS_SEG, off), b);
    put(CAVE_LOAD, &[0x8d, 0x86, 0xd4, 0xfc, 0x89, 0x46, 0xf0, 0xcb]); // lea ax,[bp-32Ch]
    put(CAVE_SAVE, &[0x8d, 0x86, 0xf0, 0xfc, 0x89, 0x46, 0xf0, 0xcb]); // lea ax,[bp-310h]
    let f = le(FLAG_BATTLE);
    put(CAVE_MENU_RET, &[
        0x1e, 0x6a, 0x00, 0x1f, // push ds; push 0; pop ds
        0xc6, 0x06, f[0], f[1], 0x00, // mov byte [FLAG_BATTLE],0
        0x1f, // pop ds
        0xea, 0x07, 0x3b, bat_lo, bat_hi, // jmp 2687:3b07
    ]);
    // The menu draws item texts from DS only (it ignores the far pointers'
    // segment), so texts go into the frame too (SS = DS), and the pointers to them
    // are set to the copy.
    let (t, a) = (le(MENU_TEMPLATE), le(MENU_ADDR));
    put(CAVE_MENU_TABLE, &[
        0x8b, 0xc2, // mov ax,dx
        0x8d, 0x5e, 0x8e, // lea bx,[bp-72h]
        0x9c, 0x06, 0x56, 0x57, 0x51, // pushf; push es/si/di/cx
        0x16, 0x07, // push ss; pop es
        0x8b, 0xfb, // mov di,bx
        0xbe, t[0], t[1], // mov si,MENU_TEMPLATE
        0xb9, (TEMPLATE_LEN / 2) as u8, 0x00, // mov cx,TEMPLATE_LEN/2
        0xfc, // cld
        0x2e, 0xf3, 0xa5, // rep movsw cs:[si] -> es:[di]
        0x8d, 0x77, T_SAVE as u8, 0x89, 0x77, 0x20, 0x8c, 0x57, 0x22, // item 8 -> ss:bx+T_SAVE
        0x8d, 0x77, T_LOAD as u8, 0x89, 0x77, 0x24, 0x8c, 0x57, 0x26, // item 9 -> ss:bx+T_LOAD
        0x1e, 0x6a, 0x00, 0x1f, 0x89, 0x1e, a[0], a[1], 0x1f, // push ds; push 0; pop ds; mov [MENU_ADDR],bx; pop ds
        0x59, 0x5f, 0x5e, 0x07, 0x9d, // pop cx/di/si/es; popf
        0xcb, // retf
    ]);
    // The game's ten items (relocated far pointers) with 存檔 / 讀檔 before 遊戲結束.
    let orig: Vec<[u8; 4]> = (0..10u16).map(|i| {
        let a = lin(ds, MENU_TABLE + 4 * i) as usize;
        [m.mem.ram[a], m.mem.ram[a + 1], m.mem.ram[a + 2], m.mem.ram[a + 3]]
    }).collect();
    let mut table: Vec<u8> = Vec::new();
    for e in orig[..8].iter().chain([[0u8; 4], [0u8; 4]].iter()).chain(orig[8..].iter()) {
        table.extend_from_slice(e); // items 8 and 9 are pointed at their texts by the routine
    }
    table.extend_from_slice(&[0; 4]);
    assert_eq!(table.len(), T_SAVE as usize);
    for (at, s) in [(T_SAVE, "存檔"), (T_LOAD, "讀檔"), (T_OLD_SAVE, "這個進度請在標題畫面讀取")] {
        table.resize(at as usize, 0);
        table.extend_from_slice(&big5(s));
        table.push(0);
    }
    table.resize(TEMPLATE_LEN as usize, 0);
    let mut put = |off: u16, b: &[u8]| m.mem.write_bytes(lin(BIOS_SEG, off), b);
    put(MENU_TEMPLATE, &table);
    put(CAVE_MSG_RET, &[0x83, 0xc4, 0x06, 0xb8, 0xff, 0xff, 0xea, 0x2b, 0xb2, ui_lo, ui_hi]);
    put(CAVE_SAVED, &[0xfe, 0x38, HOOK_SAVED, 0xea, 0x46, 0xae, ui_lo, ui_hi]);
    if m.cfg.trace {
        eprintln!("[saveui] MAIN.EXE patched: {SLOTS} save slots, 存檔/讀檔 in the battle menu");
    }
    true
}

/// Whether a callback at F000:`ip` is one of ours (the rest of F000 is BIOS stubs).
pub fn owns_bios_callback(ip: u16) -> bool {
    (CAVE_LOAD..0x0b00).contains(&ip)
}

/// Machine state of a slot (0-based) and the MSAVE header it belongs to.
pub fn state_path(game: &std::path::Path, slot: u8) -> std::path::PathBuf {
    game.join("savestates").join(format!("slot{}.state", slot + 1))
}

fn key_path(game: &std::path::Path, slot: u8) -> std::path::PathBuf {
    state_path(game, slot).with_extension("key")
}

fn save_file(game: &std::path::Path, kind: char, slot: u8) -> std::path::PathBuf {
    game.join(format!("{kind}SAVE{slot}.R3S"))
}

/// The header (date + title) that identifies one write of a save.
fn header_key(game: &std::path::Path, slot: u8) -> Option<Vec<u8>> {
    let b = std::fs::read(save_file(game, 'M', slot)).ok()?;
    (b.len() >= 0x23).then(|| b[..0x23].to_vec())
}

/// The slot's machine state, if it still belongs to the slot's save files.
pub fn fresh_state(game: &std::path::Path, slot: u8) -> Option<std::path::PathBuf> {
    let p = state_path(game, slot);
    let key = std::fs::read(key_path(game, slot)).ok()?;
    (p.exists() && header_key(game, slot)? == key).then_some(p)
}

fn flag(m: &Machine, off: u16) -> u8 {
    m.mem.ram[off as usize]
}

fn set_flag(m: &mut Machine, off: u16, v: u8) {
    m.mem.ram[off as usize] = v;
}

/// Far call from a hook: push the return address, jump to `seg:target`.
fn far_call(m: &mut Machine, seg: u16, target: u16, ret: (u16, u16)) {
    m.cpu.push16(&mut m.mem, ret.0);
    m.cpu.push16(&mut m.mem, ret.1);
    m.set_seg(CS, seg);
    m.cpu.ip = target as u32;
}

/// Continue as the far call the hook replaced would have: back to the NOPs after it.
fn replaced_call(m: &mut Machine, target: u16) {
    let (cs, ip) = (m.cpu.s[CS], m.cpu.ip as u16);
    far_call(m, cs, target, (cs, ip));
}

/// Writes are skipped where they would not belong to a person's game: the bot's
/// trial runs and replays of a journal.
fn writes_allowed(m: &Machine) -> bool {
    !m.trial && !m.bot_mode && m.cfg.replay.is_none()
}

pub fn hook(m: &mut Machine, n: u8) -> R<()> {
    let cs = m.cpu.s[CS];
    let slot = m.ax() as u8;
    match n {
        HOOK_LOAD => {
            let ui = cs;
            if let Some(p) = fresh_state(&m.cfg.game_dir, slot) {
                match m.load_state_live(&p) {
                    Ok(()) => {
                        eprintln!("[saveui] slot {}: machine state {}", slot + 1, p.display());
                        return Ok(());
                    }
                    Err(e) => eprintln!("[saveui] slot {}: {} not loaded ({e})", slot + 1, p.display()),
                }
            }
            if flag(m, FLAG_BATTLE) != 0 {
                // A save the battle cannot jump to: the game's message box (its
                // text is in the battle menu's frame, DS = SS), then the load list
                // returns as if cancelled.
                let text = m.mem.rd16(MENU_ADDR as u32).wrapping_add(T_OLD_SAVE);
                let ds = m.cpu.s[SS];
                m.cpu.push16(&mut m.mem, ds);
                m.cpu.push16(&mut m.mem, text);
                m.cpu.push16(&mut m.mem, 1);
                far_call(m, ui, 0x3242, (BIOS_SEG, CAVE_MSG_RET));
                return Ok(());
            }
            m.saveui_last = Some(slot);
            replaced_call(m, 0xa862);
        }
        HOOK_SAVE => {
            let load_seg = cs.wrapping_sub(SEG_UI);
            if flag(m, FLAG_BATTLE) != 0 {
                // From the battle menu: the runtime writes the slot (the game's
                // routine would store the campaign as of this moment, mid-battle).
                // The save list goes on after the replaced call.
                if writes_allowed(m) {
                    if let Err(e) = battle_save(m, load_seg, slot) {
                        eprintln!("[saveui] slot {} not saved: {e}", slot + 1);
                    }
                }
            } else {
                m.saveui_last = Some(slot);
                set_flag(m, SAVING_SLOT, slot);
                far_call(m, cs, 0xa3ac, (BIOS_SEG, CAVE_SAVED));
            }
        }
        HOOK_SAVED => {
            // The game wrote the slot: keep the machine with it (it continues
            // into the save list's return, F000:CAVE_SAVED+3).
            let slot = flag(m, SAVING_SLOT);
            if writes_allowed(m) {
                if let Err(e) = keep_state(m, slot) {
                    eprintln!("[saveui] slot {}: machine state not kept: {e}", slot + 1);
                }
            }
        }
        HOOK_PHASE => {
            // The player phase also returns when the battle is over or the player
            // quits (遊戲結束, 全軍撤退): only a phase ended by 回合結束 / all units
            // done is a point to come back to.
            let load_seg = cs.wrapping_sub(SEG_BATTLE);
            let ended = battle_info(m, load_seg).0;
            replaced_call(m, 0x30d6);
            if writes_allowed(m) && !ended {
                if let Err(e) = battle_save(m, load_seg, AUTO_SLOT) {
                    eprintln!("[saveui] automatic save failed: {e}");
                }
            }
        }
        HOOK_BG_SAVE => {
            // A screen redraws only its scene view (room, map) after a window
            // closes; the 10-row lists also cover the frames, so the whole
            // picture, as it was before the list's title box, is kept here and
            // put back by HOOK_BG_RESTORE.
            let planes = &m.mem.vga.planes;
            m.saveui_screen = Some(planes.iter().flat_map(|p| p[..SCREEN_PLANE].iter().copied()).collect());
            replaced_call(m, 0x2338);
        }
        HOOK_BG_RESTORE => {
            if let Some(v) = m.saveui_screen.take() {
                for (i, p) in m.mem.vga.planes.iter_mut().enumerate() {
                    p[..SCREEN_PLANE].copy_from_slice(&v[i * SCREEN_PLANE..(i + 1) * SCREEN_PLANE]);
                }
            }
            replaced_call(m, 0xc630);
        }
        HOOK_MENU => {
            let ui = cs.wrapping_sub(SEG_BATTLE).wrapping_add(SEG_UI);
            let item = m.cpu.r[0] as u8;
            if item == 8 || item == 9 {
                // 存檔 / 讀檔: the game's own save / load list, then back to the menu's exit.
                set_flag(m, FLAG_BATTLE, 1);
                m.set_ax(0x100);
                far_call(m, ui, if item == 8 { 0xb2ce } else { 0xb2c2 }, (BIOS_SEG, CAVE_MENU_RET));
                return Ok(());
            }
            // The game's numbering (遊戲結束 = 8, DEBUG = 9), then what the replaced
            // `mov [bp-1],al; cmp al,8` did.
            let al = match item { 10 => 8, 11 => 9, x => x };
            m.set_al(al);
            let bp = m.cpu.r[5] as u16;
            let a = lin(m.cpu.s[SS], bp.wrapping_sub(1));
            m.mem.ram[a as usize] = al;
            m.cpu.fl &= !(CF | ZF);
            if al < 8 {
                m.cpu.fl |= CF;
            }
            if al == 8 {
                m.cpu.fl |= ZF;
            }
        }
        _ => {
            return Err(format!("unknown hook {n:02x} at {cs:04x}:{:04x}", m.cpu.op_ip));
        }
    }
    Ok(())
}

/// Whether the battle is over (+1 nonzero: won, lost, quit — the round loop at
/// 2687:3d1d stops on it), the round number (1-based, as the game shows it) and
/// the battle name.
fn battle_info(m: &Machine, load_seg: u16) -> (bool, u32, Vec<u8>) {
    let b = lin(load_seg.wrapping_add(SEG_DATA), BATTLE);
    let ended = m.mem.ram[b as usize + 1] != 0;
    let round = m.mem.ram[b as usize + 2] as u32 + 1;
    let name: Vec<u8> = (0..24).map(|i| m.mem.ram[(b + 6 + i) as usize]).take_while(|&c| c != 0).collect();
    (ended, round, name)
}

/// A list row title: "<battle> 第N回合" in Big5, cut at a whole character so it
/// fits the 30-byte title field (29 bytes + NUL).
pub fn title(battle: &[u8], round: u32) -> Vec<u8> {
    let suffix = big5(&format!(" 第{round}回合"));
    let mut name = Vec::new();
    let mut i = 0;
    while i < battle.len() {
        let w = if battle[i] >= 0x81 { 2 } else { 1 };
        if name.len() + w + suffix.len() > 29 || i + w > battle.len() {
            break;
        }
        name.extend_from_slice(&battle[i..i + w]);
        i += w;
    }
    name.extend_from_slice(&suffix);
    name
}

/// Header of a save file: date and time (BCD year, month in the high nibble, BCD
/// day/hour/minute) and the title, as the game's own saves have them.
pub fn header(mut file: Vec<u8>, date: [u8; 5], title: &[u8]) -> Vec<u8> {
    assert!(file.len() >= 0x23 && title.len() < 30);
    file[..5].copy_from_slice(&date);
    file[5..0x23].fill(0);
    file[5..5 + title.len()].copy_from_slice(title);
    file
}

/// Same clock as INT 21h 2Ah/2Ch (fixed 1993-08-01 09:00 + emulated time).
fn emulated_date(m: &Machine) -> [u8; 5] {
    let mins = 9 * 60 + m.now_ms() / 60_000;
    let bcd = |v: u64| ((v / 10) << 4 | v % 10) as u8;
    [0x93, 8 << 4, 0x01, bcd(mins / 60 % 24), bcd(mins % 60)]
}

fn write_atomic(p: &std::path::Path, b: &[u8]) -> R<()> {
    crate::replace_file(p, b)
}

/// Save the machine with the slot's current files (state first, then the key
/// that ties them together).
fn keep_state(m: &mut Machine, slot: u8) -> R<()> {
    let game = m.cfg.game_dir.clone();
    let key = header_key(&game, slot).ok_or_else(|| format!("{} unreadable", save_file(&game, 'M', slot).display()))?;
    std::fs::remove_file(key_path(&game, slot)).ok();
    m.save_state_atomic(&state_path(&game, slot))?;
    write_atomic(&key_path(&game, slot), &key)
}

/// A save made in battle: MSAVE/ESAVE are a copy of the campaign save the
/// battle came from (else of any save) with the title "<battle> 第N回合", plus
/// the machine state that is the actual save.
fn battle_save(m: &mut Machine, load_seg: u16, slot: u8) -> R<()> {
    let game = m.cfg.game_dir.clone();
    let (_, round, battle) = battle_info(m, load_seg);
    let src = m.saveui_last.filter(|&s| save_file(&game, 'M', s).exists())
        .or_else(|| save_file(&game, 'M', slot).exists().then_some(slot))
        .or_else(|| (0..SLOTS).find(|&s| save_file(&game, 'M', s).exists()))
        .ok_or("no MSAVE file to copy the campaign from")?;
    let read = |p: std::path::PathBuf| std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()));
    let ms = header(read(save_file(&game, 'M', src))?, emulated_date(m), &title(&battle, round));
    if src != slot && save_file(&game, 'E', src).exists() {
        write_atomic(&save_file(&game, 'E', slot), &read(save_file(&game, 'E', src))?)?;
    }
    write_atomic(&save_file(&game, 'M', slot), &ms)?;
    keep_state(m, slot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_fits_the_field() {
        let t = title(&big5("汜水關之戰"), 12);
        assert_eq!(encoding_rs::BIG5.decode(&t).0, "汜水關之戰 第12回合");
        // A long name is cut at a whole character, never mid-way through one.
        let t = title(&big5("一二三四五六七八九十甲乙"), 30);
        assert!(t.len() <= 29);
        assert_eq!(encoding_rs::BIG5.decode(&t).0, "一二三四五六七八九十 第30回合");
    }

    #[test]
    fn header_keeps_the_save_body() {
        let mut f = vec![0x11u8; 0x40];
        f[0x23] = 0x5c;
        let h = header(f, [0x93, 0x80, 0x01, 0x09, 0x30], b"AB");
        assert_eq!(&h[..8], &[0x93, 0x80, 0x01, 0x09, 0x30, b'A', b'B', 0]);
        assert!(h[7..0x23].iter().all(|&b| b == 0));
        assert_eq!(h[0x23], 0x5c);
        assert_eq!(h[0x24], 0x11);
    }

    #[test]
    fn state_is_used_only_while_the_save_is_unchanged() {
        let g = std::env::temp_dir().join(format!("eiketsu-saveui-key-{}", std::process::id()));
        std::fs::remove_dir_all(&g).ok();
        std::fs::create_dir_all(g.join("savestates")).unwrap();
        let mut ms = vec![0u8; 0x40];
        ms[..5].copy_from_slice(&[0x93, 0x80, 0x01, 0x09, 0x05]);
        std::fs::write(save_file(&g, 'M', 3), &ms).unwrap();
        assert_eq!(fresh_state(&g, 3), None, "no state yet");
        std::fs::write(state_path(&g, 3), b"state").unwrap();
        std::fs::write(key_path(&g, 3), &ms[..0x23]).unwrap();
        assert_eq!(fresh_state(&g, 3), Some(state_path(&g, 3)));
        // The game saved into the slot again (another minute): the state is stale.
        ms[4] = 0x06;
        std::fs::write(save_file(&g, 'M', 3), &ms).unwrap();
        assert_eq!(fresh_state(&g, 3), None);
        std::fs::remove_dir_all(&g).ok();
    }

    #[test]
    fn patch_table_is_consistent() {
        for (o, old, new) in PATCHES {
            assert_eq!(old.len(), new.len(), "patch at {o:x}");
        }
        // Hooks replace far calls (9A) one for one; the menu hook replaces two instructions.
        let hooks: Vec<_> = PATCHES.iter().filter(|p| p.2[0] == 0xfe).collect();
        assert_eq!(hooks.len(), 8);
        assert_eq!(hooks.iter().filter(|p| p.1[0] == 0x9a).count(), 7);
    }
}
