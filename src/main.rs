//! eiketsu-rt: run 三國志英傑傳 (DOS) natively — no DOSBox.

use eiketsu_rt::machine::{Config, Machine, Stop};
use std::path::{Path, PathBuf};

fn find_game_dir(arg: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = arg {
        return Some(p);
    }
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let cands = [PathBuf::from("."), PathBuf::from(".."), exe_dir.join(".."), exe_dir.join("../../..")];
    cands.into_iter().find(|p| p.join("REKO3.COM").exists())
}

fn usage() -> ! {
    eprintln!("usage: eiketsu-rt [--game DIR] [--window X,Y,W,H] [--control PORT] [--speed N] [--journal FILE] [--run PROGRAM] [--headless] [--steps N] [--trace] [--shot FILE.png] [--script FILE] [--replay JOURNAL] [--no-journal] [--mute]\n       eiketsu-rt ctl PORT COMMAND...");
    std::process::exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("bot") {
        // eiketsu-rt bot GAME_DIR OUT_DIR [key=value ...]
        let (Some(game), Some(out)) = (args.get(2), args.get(3)) else { usage() };
        let mut p = eiketsu_rt::autoplay::Params::default();
        for kv in &args[4..] {
            if let Err(e) = p.set(kv) {
                eprintln!("{e}");
                std::process::exit(2);
            }
        }
        let mut b = match eiketsu_rt::autoplay::Bot::new(PathBuf::from(game), PathBuf::from(out), p) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        };
        let r = b.play();
        let line = match &r {
            Ok(total) => format!("ok total_exp_gained={total}"),
            Err(e) => format!("error {e}"),
        };
        println!("{line}");
        b.write_outputs(&line).ok();
        std::process::exit(if r.is_ok() { 0 } else { 1 });
    }
    if args.get(1).map(String::as_str) == Some("walk") {
        // eiketsu-rt walk GAME_DIR OUT_DIR STATE STEPS
        let (Some(game), Some(out), Some(state), Some(steps)) = (args.get(2), args.get(3), args.get(4), args.get(5)) else { usage() };
        let mut b = eiketsu_rt::autoplay::Bot::new(PathBuf::from(game), PathBuf::from(out), Default::default()).unwrap_or_else(|e| { eprintln!("{e}"); std::process::exit(1) });
        if let Err(e) = b.walk(&PathBuf::from(state), steps) { eprintln!("{e}"); std::process::exit(1) }
        std::process::exit(0);
    }
    if args.get(1).map(String::as_str) == Some("ctl") {
        let port = args.get(2).and_then(|p| p.parse().ok()).unwrap_or_else(|| usage());
        std::process::exit(eiketsu_rt::control::client(port, &args[3..].join(" ")));
    }
    let mut game = None;
    let mut window = None;
    let mut control = None;
    let mut speed = 1;
    let mut journal_path: Option<PathBuf> = None;
    let (mut headless, mut trace, mut no_journal, mut mute) = (false, false, false, false);
    let mut replay = None;
    let mut steps = None;
    let mut shot: Option<PathBuf> = None;
    let mut program = r"C:\REKO3.COM".to_string();
    let mut script = Vec::new();
    let mut battle_log: Option<PathBuf> = None;
    let mut debug_grid = false;
    let mut state: Option<PathBuf> = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--game" => game = it.next().map(PathBuf::from),
            "--window" => {
                // X,Y,W,H
                let v: Vec<i64> = it.next().unwrap_or_else(|| usage()).split(',').filter_map(|s| s.parse().ok()).collect();
                if v.len() != 4 {
                    usage();
                }
                window = Some((v[0] as i32, v[1] as i32, v[2] as usize, v[3] as usize));
            }
            "--control" => control = it.next().and_then(|s| s.parse().ok()),
            "--speed" => speed = it.next().and_then(|s| s.parse().ok()).unwrap_or(1),
            "--journal" => journal_path = it.next().map(PathBuf::from),
            "--run" => program = it.next().unwrap_or_else(|| usage()),
            "--headless" => headless = true,
            "--trace" => trace = true,
            "--mute" => mute = true,
            "--replay" => replay = it.next().map(PathBuf::from),
            "--no-journal" => no_journal = true,
            "--battle-log" => battle_log = it.next().map(PathBuf::from),
            "--debug" => debug_grid = true,
            "--state" => state = it.next().map(PathBuf::from),
            "--steps" => steps = it.next().and_then(|s| s.parse().ok()),
            "--shot" => shot = it.next().map(PathBuf::from),
            "--script" => {
                let p = PathBuf::from(it.next().unwrap_or_else(|| usage()));
                let text = std::fs::read_to_string(&p).unwrap_or_else(|e| {
                    eprintln!("{}: {e}", p.display());
                    std::process::exit(2)
                });
                let base = p.parent().map(Path::to_path_buf).unwrap_or_default();
                script = eiketsu_rt::script::parse(&text, &base).unwrap_or_else(|e| {
                    eprintln!("{e}");
                    std::process::exit(2)
                });
            }
            _ => usage(),
        }
    }
    let Some(game_dir) = find_game_dir(game) else {
        eprintln!("cannot find the game folder (with REKO3.COM); pass --game DIR");
        std::process::exit(2);
    };
    let cfg = Config {
        journal: journal_path.or_else(|| (!headless && !no_journal && replay.is_none()).then(|| {
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            game_dir.join("journal").join(format!("session-{t}.txt"))
        })),
        game_dir,
        headless,
        trace,
        replay,
        max_steps: steps,
        script,
        program,
        audio: !mute,
        window,
        control,
        speed,
        battle_log,
        debug_grid,
    };
    let mut m = match Machine::new(cfg) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("load failed: {e}");
            std::process::exit(1);
        }
    };
    if let Some(p) = &state {
        // Start from a save state (e.g. a battle's first turn); a journal recorded
        // this way replays with the same --state.
        // A --script given with it runs on the state's clock: its times count
        // from the moment the state was saved.
        let script = std::mem::take(&mut m.cfg.script);
        if let Err(e) = m.load_state(p) {
            eprintln!("{e}");
            std::process::exit(1);
        }
        let t0 = m.now_ms();
        m.cfg.script = script.into_iter().map(|mut e| { e.ms += t0; e }).collect();
    }
    if let Some(j) = &m.cfg.journal {
        eprintln!("[journal] {}", j.display());
    }
    let stop = m.run();
    if let Some(p) = shot {
        if let Err(e) = std::fs::write(&p, m.screenshot_png()) {
            eprintln!("screenshot: {e}");
        }
    }
    let code = match stop {
        Stop::Exit(c) => c,
        Stop::Closed => 0,
        Stop::Budget => {
            eprintln!(
                "[stop] step budget reached at {} steps ({} ms); cs:ip={:04x}:{:04x} in {}; {} OPL writes",
                m.cpu.steps, m.now_ms(), m.cpu.s[eiketsu_rt::cpu::CS], m.cpu.ip, m.dos.current_name(), m.mem.opl.writes
            );
            0
        }
        Stop::Error(e) => {
            eprintln!("[error] {e}");
            1
        }
    };
    std::process::exit(code);
}
