//! Input journal: every input a windowed session receives, keyed by the instruction
//! count it was applied at. Emulation is deterministic (time comes from the
//! instruction count, the clock/date start at a fixed epoch), so replaying a journal
//! reproduces the session exactly.
//!
//! Format: `<step> <kind> <args…>`; `#` starts a comment.
//!   mouse X Y BUTTONS | key SCANCODE(hex, 1xx = E0-prefixed) 0|1 | close

use crate::host::Event;
use crate::IPS_PER_MS;
use std::collections::VecDeque;
use std::io::Write;
use std::path::Path;

pub fn encode(e: &Event) -> String {
    match e {
        Event::Close => "close".into(),
        Event::Mouse { x, y, buttons } => format!("mouse {x} {y} {buttons}"),
        Event::Key { sc, down } => format!("key {sc:x} {}", *down as u8),
    }
}

fn decode(t: &[&str]) -> Option<Event> {
    let n = |i: usize| t.get(i).and_then(|s| s.parse::<i32>().ok());
    Some(match *t.first()? {
        "close" => Event::Close,
        "mouse" => Event::Mouse { x: n(1)?, y: n(2)?, buttons: n(3)? as u16 },
        "key" => Event::Key { sc: u16::from_str_radix(t.get(1)?, 16).ok()?, down: n(2)? != 0 },
        _ => return None,
    })
}

pub fn parse_line(line: &str) -> Option<(u64, Event)> {
    let line = line.split('#').next()?.trim();
    if line.is_empty() {
        return None;
    }
    let t: Vec<&str> = line.split_whitespace().collect();
    let step: u64 = t.first()?.parse().ok()?;
    Some((step, decode(&t[1..])?))
}

pub fn load(p: &Path) -> Result<VecDeque<(u64, Event)>, String> {
    let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
    let mut out = VecDeque::new();
    for (no, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap().trim();
        if line.is_empty() {
            continue;
        }
        let t: Vec<&str> = line.split_whitespace().collect();
        let bad = || format!("{} line {}: {line}", p.display(), no + 1);
        let step: u64 = t[0].parse().map_err(|_| bad())?;
        out.push_back((step, decode(&t[1..]).ok_or_else(bad)?));
    }
    Ok(out)
}

pub fn create(p: &Path) -> Result<std::fs::File, String> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let mut f = std::fs::File::create(p).map_err(|e| format!("{}: {e}", p.display()))?;
    writeln!(f, "# eiketsu-rt input journal (ips/ms={IPS_PER_MS})").ok();
    Ok(f)
}
