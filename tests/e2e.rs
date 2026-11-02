//! End-to-end acceptance harness: runs the original game (REKO3.COM → MAIN.EXE)
//! headless on the deterministic virtual clock and checks screen hashes at
//! scripted checkpoints.
//!
//! Each `harness/*.txt` is one scenario. Every scenario runs on a fresh copy of the
//! game folder, so saves it writes never touch the real one and runs repeat
//! exactly. Golden hashes were recorded after a human looked at the matching
//! screenshot; the actual frame is always written to `harness/out/<scenario>/`.

use eiketsu_rt::machine::{Config, Machine, Stop};
use eiketsu_rt::script::{parse, ScriptAction, ScriptEvent};
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Copy the game's own files (not this project, not journals) to a scratch folder.
fn game_copy(tag: &str) -> PathBuf {
    let src = root().join("..");
    assert!(src.join("REKO3.COM").exists(), "game data missing at {}", src.display());
    let dst = std::env::temp_dir().join(format!("eiketsu-e2e-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&dst).ok();
    std::fs::create_dir_all(&dst).unwrap();
    for e in std::fs::read_dir(&src).unwrap().flatten() {
        if e.file_type().unwrap().is_file() {
            std::fs::copy(e.path(), dst.join(e.file_name())).unwrap();
        }
    }
    dst
}

/// A game copy whose saves are fixed: slot 2 is the 汜水關 pre-battle save in
/// harness/fixtures/sishui_hall (in 陳留's hall, before talking to 關羽), slot 1
/// the game's original save, slots 3-10 empty — whatever the player's own folder
/// holds by now.
fn game_copy_sishui(tag: &str) -> PathBuf {
    let g = game_copy(tag);
    for slot in 1..10 {
        for k in ['M', 'E'] {
            std::fs::remove_file(g.join(format!("{k}SAVE{slot}.R3S"))).ok();
        }
    }
    for f in ["MSAVE1.R3S", "ESAVE1.R3S"] {
        std::fs::copy(root().join("harness/fixtures/sishui_hall").join(f), g.join(f)).unwrap();
    }
    g
}

fn config(game: PathBuf, script: Vec<ScriptEvent>, journal: Option<PathBuf>, replay: Option<PathBuf>, max_steps: u64) -> Config {
    Config {
        game_dir: game,
        headless: true,
        trace: false,
        journal,
        replay,
        max_steps: Some(max_steps),
        script,
        program: r"C:\REKO3.COM".into(),
        audio: false,
        window: None,
        control: None,
        speed: 1,
        battle_log: None,
        debug_grid: false,
    }
}

fn run_scenario(file: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(file).unwrap();
    let name = file.file_stem().unwrap().to_string_lossy().to_string();
    let out = root().join("harness/out").join(&name);
    std::fs::create_dir_all(&out).unwrap();
    let mut script = parse(&text, &root().join("harness")).unwrap();
    let shots: Vec<ScriptEvent> = script.iter().filter_map(|e| match &e.action {
        ScriptAction::Expect(n, _) => Some(ScriptEvent { ms: e.ms, action: ScriptAction::Shot(out.join(format!("{n}.png"))) }),
        _ => None,
    }).collect();
    script.extend(shots);
    script.sort_by_key(|e| e.ms);
    let game = game_copy(&name);
    let budget = (script.last().map_or(0, |e| e.ms) + 10_000) * eiketsu_rt::IPS_PER_MS;
    let mut m = Machine::new(config(game.clone(), script, None, None, budget)).unwrap();
    let stop = m.run();
    std::fs::remove_dir_all(&game).ok();
    let mut fails = Vec::new();
    if !matches!(stop, Stop::Exit(0)) {
        fails.push(format!("{name}: run ended with {stop:?}"));
    }
    // A checkpoint the run never reached is a failure, not a pass.
    for e in &m.cfg.script {
        if let ScriptAction::Expect(cp, _) | ScriptAction::ExpectFile(cp, ..) = &e.action {
            fails.push(format!("{name}/{cp}: checkpoint at {} ms was never reached", e.ms));
        }
    }
    for (cp, want, got) in &m.checks {
        if want != got {
            fails.push(format!("{name}/{cp}: expected {want:016x}, got {got:016x} (see harness/out/{name}/{cp}.png)"));
        }
    }
    fails
}

#[test]
fn harness() {
    let mut files: Vec<PathBuf> = std::fs::read_dir(root().join("harness")).unwrap()
        .flatten().map(|e| e.path()).filter(|p| p.extension().map_or(false, |x| x == "txt")).collect();
    files.sort();
    let filter = std::env::var("EIKETSU_CASE").ok();
    let mut fails = Vec::new();
    for f in files {
        if filter.as_ref().map_or(false, |c| !f.to_string_lossy().contains(c.as_str())) {
            continue;
        }
        fails.extend(run_scenario(&f));
    }
    assert!(fails.is_empty(), "harness failures:\n{}", fails.join("\n"));
}

/// The bug-report path: a session's journal, replayed with no other input, must
/// reach exactly the same state (same screen, same registers, same step).
#[test]
fn journal_replay_reproduces_session() {
    let out = root().join("harness/out/journal");
    std::fs::create_dir_all(&out).unwrap();
    let journal = out.join("session.txt");
    let script = parse("9000 key A\n9600 key ENTER\n14000 move 200 120\n15000 click 320 187\n", &out).unwrap();
    let steps = 22_000 * eiketsu_rt::IPS_PER_MS;
    let ga = game_copy("journal-a");
    let mut a = Machine::new(config(ga.clone(), script, Some(journal.clone()), None, steps)).unwrap();
    assert!(matches!(a.run(), Stop::Budget));
    drop(a.cfg.journal.take());
    let gb = game_copy("journal-b");
    let mut b = Machine::new(config(gb.clone(), Vec::new(), None, Some(journal.clone()), steps)).unwrap();
    assert!(matches!(b.run(), Stop::Budget));
    std::fs::remove_dir_all(&ga).ok();
    std::fs::remove_dir_all(&gb).ok();
    let text = std::fs::read_to_string(&journal).unwrap();
    assert!(text.lines().filter(|l| l.contains(" mouse ") || l.contains(" key ")).count() >= 6, "journal too short:\n{text}");
    assert_eq!(a.screen_hash(), b.screen_hash(), "replay diverged from the recorded session");
    assert_eq!(a.cpu.r, b.cpu.r, "CPU registers differ after replay");
}

/// Remote control (what agents use to play a window): input sent over the control
/// port reaches the game, is journaled, and `shot` writes the current picture.
#[test]
fn control_port_drives_the_game() {
    let out = root().join("harness/out/control");
    std::fs::create_dir_all(&out).unwrap();
    let journal = out.join("session.txt");
    let shot = out.join("after_password.png");
    let game = game_copy("control");
    let mut cfg = config(game.clone(), Vec::new(), Some(journal.clone()), None, 16_000 * eiketsu_rt::IPS_PER_MS);
    cfg.control = Some(7790);
    let mut m = Machine::new(cfg).unwrap();
    let shot2 = shot.clone();
    let client = std::thread::spawn(move || {
        let send = |c: &str| eiketsu_rt::control::client(7790, c);
        // Headless runs ~4x real time: wait in emulated ms, not wall time.
        assert_eq!(send("wait 9000"), 0);
        assert_eq!(send("type a"), 0);
        assert_eq!(send("wait 400"), 0);
        assert_eq!(send("key ENTER"), 0);
        assert_eq!(send("wait 4500"), 0);
        assert_eq!(send(&format!("shot {}", shot2.display())), 0);
        assert_eq!(send("bogus"), 1);
    });
    assert!(matches!(m.run(), Stop::Budget));
    client.join().unwrap();
    std::fs::remove_dir_all(&game).ok();
    let text = std::fs::read_to_string(&journal).unwrap();
    assert!(text.contains(" key 1e 1") && text.contains(" key 1c 1"), "control input not journaled:\n{text}");
    // Golden: the title menu (same frame as harness 01's title_menu, checked by eye).
    let png = std::fs::read(&shot).unwrap();
    assert!(png.len() > 1000);
    assert_eq!(format!("{:016x}", m.screen_hash()), "093a158fdd5a2c4b".to_string(), "not on the title menu");
}

/// `state` reads the battle from memory. Uses save slot 2 of the fixture (the
/// 汜水關 save): talk to 關羽 only, skip the opening dialog, read the field.
/// Golden values were checked against the screenshot (harness/out/state/field.png):
/// only 劉備/關羽/張飛 on our side, 劉備 at (22,9) with 500 troops; enemies as the
/// in-game 敵軍 tab lists them (華雄 740 LV5, 李肅 540, 胡軫 550, 趙岑 550, 3 步兵隊 500).
#[test]
fn state_reads_the_battlefield() {
    let out = root().join("harness/out/state");
    std::fs::create_dir_all(&out).unwrap();
    let mut text = String::from(
        "9000 key A\n9400 key ENTER\n14000 click 320 211\n16500 click 220 200\n18000 click 350 231\n\
         25000 click 222 238\n26500 click 222 238\n29000 click 382 152\n32000 click 295 231\n",
    );
    for i in 0..18 {
        text.push_str(&format!("{} click 320 300\n", 35_000 + i * 2_500));
    }
    text.push_str("83000 shot field.png\n");
    let script = parse(&text, &out).unwrap();
    let game = game_copy_sishui("state");
    let mut m = Machine::new(config(game.clone(), script, None, None, 83_100 * eiketsu_rt::IPS_PER_MS)).unwrap();
    assert!(matches!(m.run(), Stop::Budget));
    std::fs::remove_dir_all(&game).ok();
    let units = eiketsu_rt::battle::units(&m.mem).expect("no battle data");
    let ours: Vec<_> = units.iter().filter(|u| u.side == "我軍").map(|u| u.name.as_str()).collect();
    assert_eq!(ours, ["劉備", "關羽", "張飛"]);
    let enemies: Vec<_> = units.iter().filter(|u| u.side == "敵軍").map(|u| (u.name.as_str(), u.troops, u.level)).collect();
    assert_eq!(enemies, [("華雄", 740, 5), ("李肅", 540, 2), ("胡軫", 550, 2), ("趙岑", 550, 2), ("步兵隊", 500, 1), ("步兵隊", 500, 1), ("步兵隊", 500, 1)]);
    // View scroll (13,3) tiles: 劉備's tile (22,9) is drawn at (320,280).
    assert_eq!(eiketsu_rt::battle::view(&m.mem), Some((26, 6)));
    assert_eq!(eiketsu_rt::battle::tile_screen(&m.mem, 22, 9), Some((320, 280)));
    let liu = &units[0];
    assert_eq!((liu.x, liu.y, liu.troops, liu.level, liu.exp), (22, 9, 500, 1, 0));
}

/// A script with a screenshot (`<checkpoint>.png` in `out`) at every `expect`.
fn with_shots(text: &str, out: &Path) -> Vec<ScriptEvent> {
    let mut script = parse(text, out).unwrap();
    let shots: Vec<_> = script.iter().filter_map(|e| match &e.action {
        ScriptAction::Expect(n, _) => Some(ScriptEvent { ms: e.ms, action: ScriptAction::Shot(out.join(format!("{n}.png"))) }),
        _ => None,
    }).collect();
    script.extend(shots);
    script.sort_by_key(|e| e.ms);
    script
}

/// Where every unit stands and how strong it is (from memory, not the screen).
fn field(m: &Machine) -> Vec<String> {
    eiketsu_rt::battle::units(&m.mem).expect("no battle data").iter()
        .map(|u| format!("{}{} ({},{}) 兵{} LV{} EXP{}", u.side, u.name, u.x, u.y, u.troops, u.level, u.exp)).collect()
}

/// Battle round as stored by the game (0-based; DS:CF6A+2 with MAIN at 0995).
fn round(m: &Machine) -> u8 {
    m.mem.ram[0x3a120 + 0xcf6a + 2]
}

/// The game's own save screens with 10 slots, and slot 10 = automatic save at the
/// end of every player phase. Real path, real files:
/// load slot 2 (fixture: 汜水關 hall) → 「現在儲存嗎？」是 → save list (10 rows) →
/// slot 7 → battle → 功能/回合結束 → enemies move → 功能/遊戲結束 (must NOT
/// autosave) → title → load list shows slots 7 and 10 → load slot 10 → back on
/// the field right after round 1's player phase, and the enemy phase replays the same.
///
/// Expected header bytes are written out by hand from what the game itself stores
/// (BCD year, month in the high nibble, BCD day/hour/minute, Big5 title).
#[test]
fn save_screens_have_ten_slots_and_slot_10_follows_the_battle() {
    let out = root().join("harness/out/saveui");
    std::fs::create_dir_all(&out).unwrap();
    let mut text = String::from(
        "9000 key A\n9400 key ENTER\n14000 click 320 211\n16500 click 220 200\n18000 click 350 231\n\
         25000 click 222 238\n26500 click 222 238\n29000 click 382 152\n32000 click 222 231\n\
         34500 expect save_list_10_rows d3cdc8472af8606b\n35000 click 280 316\n37000 click 290 231\n",
    );
    for i in 0..18 {
        text.push_str(&format!("{} click 320 300\n", 40_000 + i * 2_500));
    }
    // 功能 → 回合結束 → 是: round 1's player phase ends here (autosave).
    text.push_str("86000 click 45 55\n87000 click 85 110\n88000 click 222 232\n");
    let script = with_shots(&text, &out);
    let game = game_copy_sishui("saveui");
    let mut m = Machine::new(config(game.clone(), script, None, None, 96_000 * eiketsu_rt::IPS_PER_MS)).unwrap();
    assert!(matches!(m.run(), Stop::Budget));
    std::fs::write(out.join("enemy_done.png"), m.screenshot_png()).unwrap();
    let read = |f: &str| std::fs::read(game.join(f)).unwrap_or_else(|e| panic!("{f}: {e}"));

    // Slot 7 was written by the game itself (0-based file number 6).
    let m6 = read("MSAVE6.R3S");
    assert_eq!(&m6[..5], &[0x93, 0x80, 0x01, 0x09, 0x00], "slot 7 date 93/08/01 09:00");
    assert_eq!(m6.len(), 8789);
    assert_eq!(read("ESAVE6.R3S").len(), 1369);
    // Slot 10: machine state + the list row, copied from the save the battle came from.
    assert!(game.join("savestates/slot10.state").exists() && game.join("savestates/slot10.key").exists());
    assert!(game.join("savestates/slot7.state").exists(), "a save made in the hall keeps its machine state too");
    let m9 = read("MSAVE9.R3S");
    let title_r1: &[u8] = &[0xc9, 0xfa, 0xa4, 0xf4, 0xc3, 0xf6, 0xa4, 0xa7, 0xbe, 0xd4, 0x20, 0xb2, 0xc4, 0x31, 0xa6, 0x5e, 0xa6, 0x58, 0x00]; // 汜水關之戰 第1回合
    assert_eq!(&m9[..5], &[0x93, 0x80, 0x01, 0x09, 0x01], "slot 10 date 93/08/01 09:01");
    assert_eq!(&m9[5..5 + title_r1.len()], title_r1);
    assert_eq!(&m9[0x23..], &m6[0x23..], "slot 10 carries slot 7's campaign data");
    assert_eq!(read("ESAVE9.R3S"), read("ESAVE6.R3S"));
    assert_eq!(round(&m), 1, "enemy phase done, round 2 under way");
    let field_after_enemies = field(&m);

    // Quit from the 功能 menu (遊戲結束, last row since 存檔/讀檔 were added; no
    // autosave), back to the title, load slot 10.
    let more = "97000 click 45 55\n98000 click 85 350\n99500 click 180 231\n113000 click 180 231\n\
                118500 click 320 211\n121000 expect load_list_10_rows 37a219af89ff3c08\n121500 click 320 388\n123500 click 350 231\n";
    m.cfg.script = with_shots(more, &out);
    // Loading slot 10 puts the clock back to ~88 s (round 1's player phase end);
    // the run then goes on to the step budget: the enemy phase plays again and
    // round 2 waits for input. Without the load we would be at the title, with
    // no battle in memory at all.
    m.cfg.max_steps = Some(124_000 * eiketsu_rt::IPS_PER_MS);
    assert!(matches!(m.run(), Stop::Budget));
    std::fs::write(out.join("after_load.png"), m.screenshot_png()).unwrap();
    assert_eq!(&read("MSAVE9.R3S")[5..5 + title_r1.len()], title_r1, "quitting must not overwrite slot 10");
    std::fs::remove_dir_all(&game).ok();
    assert!(eiketsu_rt::battle::units(&m.mem).is_some(), "slot 10 did not bring the battle back");
    assert_eq!(round(&m), 1, "round 2 under way again");
    assert_eq!(field(&m), field_after_enemies, "the enemy phase after loading slot 10 differs");

    let fails: Vec<String> = m.checks.iter().filter(|(_, w, g)| w != g)
        .map(|(cp, w, g)| format!("{cp}: expected {w:016x}, got {g:016x} (see harness/out/saveui/{cp}.png)")).collect();
    assert_eq!(m.checks.len(), 2, "a screen checkpoint was not reached");
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

/// Runs `script` (absolute emulated ms) on `m` up to `until_ms` of the step
/// budget. A load rewinds the clock; the run still stops at the same step count.
fn run_until(m: &mut Machine, out: &Path, script: &str, until_ms: u64) {
    m.cfg.script = with_shots(script, out);
    m.cfg.max_steps = Some(until_ms * eiketsu_rt::IPS_PER_MS);
    assert!(matches!(m.run(), Stop::Budget));
}

/// 存檔 / 讀檔 in the battle 功能 menu (any time in battle):
/// hall: save slot 7 → battle: 功能/存檔 slot 4 → end the round → 功能/讀檔
/// slot 4 = back to that moment → 功能/讀檔 slot 1 (the game's original save, no
/// machine state) = the game's message, still in battle → 功能/讀檔 slot 7 =
/// back in the hall. Facts come from memory and the files written.
#[test]
fn battle_menu_saves_and_loads_any_time() {
    let out = root().join("harness/out/battlemenu");
    std::fs::create_dir_all(&out).unwrap();
    let game = game_copy_sishui("battlemenu");
    let mut text = String::from(
        "9000 key A\n9400 key ENTER\n14000 click 320 211\n16500 click 220 200\n18000 click 350 231\n\
         25000 click 222 238\n26500 click 222 238\n29000 click 382 152\n32000 click 222 231\n\
         35000 click 280 316\n37000 click 290 231\n",
    );
    for i in 0..18 {
        text.push_str(&format!("{} click 320 300\n", 40_000 + i * 2_500));
    }
    // 功能 → 存檔 → slot 4 → 是.
    text.push_str("86000 click 45 55\n87000 expect menu_with_save_load ffe07c0fcbd5eac0\n87500 click 85 302\n89000 click 280 244\n90500 click 232 231\n92500 expect after_battle_save cbcce1551607590b\n");
    let mut m = Machine::new(config(game.clone(), with_shots(&text, &out), None, None, 93_000 * eiketsu_rt::IPS_PER_MS)).unwrap();
    assert!(matches!(m.run(), Stop::Budget));
    let read = |f: &str| std::fs::read(game.join(f)).unwrap_or_else(|e| panic!("{f}: {e}"));
    let m3 = read("MSAVE3.R3S");
    let title_r1: &[u8] = &[0xc9, 0xfa, 0xa4, 0xf4, 0xc3, 0xf6, 0xa4, 0xa7, 0xbe, 0xd4, 0x20, 0xb2, 0xc4, 0x31, 0xa6, 0x5e, 0xa6, 0x58, 0x00]; // 汜水關之戰 第1回合
    assert_eq!(&m3[5..5 + title_r1.len()], title_r1);
    assert_eq!(&read("savestates/slot4.key")[..], &m3[..0x23], "slot 4's state belongs to this MSAVE3");
    assert_eq!(round(&m), 0);
    assert_eq!(m.mem.ram[0x4f0], 0, "battle-menu flag cleared after the list");
    let field_saved = field(&m);

    // End round 1. After a menu action the first click on 功能 shows the army's
    // range (the game's own behaviour): right-click it away, click again.
    run_until(&mut m, &out, "94000 click 45 55\n95000 rclick 300 300\n96000 click 45 55\n97000 click 85 110\n98000 click 222 232\n", 106_000);
    assert_eq!(round(&m), 1, "round 2 under way");

    // 功能 → 讀檔 → slot 4: back to the moment of the save (clock ~90.5 s). At the
    // start of a round the first click on 功能 opens the menu.
    run_until(&mut m, &out, "107000 click 45 55\n108000 click 85 326\n\
                             110000 expect battle_load_list f9b8b28f540fb98a\n110500 click 280 244\n112000 click 247 231\n", 116_000);
    assert_eq!(round(&m), 0, "slot 4 not loaded: not back in round 1");
    assert_eq!(field(&m), field_saved);
    assert_eq!(m.mem.ram[0x4f0], 0);

    // 功能 → 讀檔 → slot 1 (no machine state): the game's message, battle goes on.
    // (Right after a load the first click on 功能 opens the menu.)
    run_until(&mut m, &out, "117000 click 45 55\n118000 click 85 326\n\
                             120000 click 280 172\n121500 click 247 231\n123500 expect old_save_message e60489acfd8e95e0\n124000 click 272 231\n", 128_000);
    assert_eq!(round(&m), 0);
    assert_eq!(field(&m), field_saved, "the old save must not have been loaded into the battle");
    assert_eq!(m.mem.ram[0x4f0], 0);

    // 功能 → 讀檔 → slot 7 (saved in the hall by this build): back in the hall.
    run_until(&mut m, &out, "129000 click 45 55\n130000 rclick 300 300\n131000 click 45 55\n132000 click 85 326\n\
                             134000 click 280 316\n135500 click 247 231\n139000 expect hall_again 31bcf098b6299709\n", 140_000);
    std::fs::write(out.join("hall_again.png"), m.screenshot_png()).unwrap();
    std::fs::remove_dir_all(&game).ok();
    let fails: Vec<String> = m.checks.iter().filter(|(_, w, g)| w != g)
        .map(|(cp, w, g)| format!("{cp}: expected {w:016x}, got {g:016x} (see harness/out/battlemenu/{cp}.png)")).collect();
    assert_eq!(m.checks.len(), 5, "a screen checkpoint was not reached");
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}
