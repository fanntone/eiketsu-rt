//! Harness scripts: `<ms> <action>` per line, `#` starts a comment. Times are
//! emulated milliseconds.
//!
//!   click X Y | rclick X Y | move X Y      mouse in driver coordinates (x 0-639)
//!   key NAME                               press and release (`key ENTER`, `key F1`)
//!   shot FILE | expect NAME HASH | expectfile WRITTEN REFERENCE | quit

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub enum ScriptAction {
    Mouse(i32, i32, u16),
    Key(u16, bool),
    Shot(PathBuf),
    Expect(String, u64),
    /// (checkpoint name, file written by the game relative to the game folder, reference file)
    ExpectFile(String, String, PathBuf),
    Quit,
}

#[derive(Clone, Debug)]
pub struct ScriptEvent {
    pub ms: u64,
    pub action: ScriptAction,
}

pub fn parse(text: &str, base: &Path) -> Result<Vec<ScriptEvent>, String> {
    let mut out = Vec::new();
    for (no, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap().trim();
        if line.is_empty() {
            continue;
        }
        let t: Vec<&str> = line.split_whitespace().collect();
        let bad = || format!("script line {}: {line}", no + 1);
        let ms: u64 = t[0].parse().map_err(|_| bad())?;
        let num = |i: usize| t.get(i).and_then(|s| s.parse::<i32>().ok()).ok_or_else(bad);
        let mut push = |ms: u64, a: ScriptAction| out.push(ScriptEvent { ms, action: a });
        match *t.get(1).ok_or_else(bad)? {
            "move" => push(ms, ScriptAction::Mouse(num(2)?, num(3)?, 0)),
            k @ ("click" | "rclick") => {
                let (x, y) = (num(2)?, num(3)?);
                let b = if k == "click" { 1 } else { 2 };
                push(ms, ScriptAction::Mouse(x, y, 0));
                push(ms + 30, ScriptAction::Mouse(x, y, b));
                push(ms + 150, ScriptAction::Mouse(x, y, 0));
            }
            "key" => {
                let sc = crate::keys::by_name(t.get(2).ok_or_else(bad)?).ok_or_else(bad)?;
                push(ms, ScriptAction::Key(sc, true));
                push(ms + 80, ScriptAction::Key(sc, false));
            }
            "shot" => push(ms, ScriptAction::Shot(base.join(t.get(2).ok_or_else(bad)?))),
            "expect" => {
                let h = u64::from_str_radix(t.get(3).ok_or_else(bad)?, 16).map_err(|_| bad())?;
                push(ms, ScriptAction::Expect(t.get(2).ok_or_else(bad)?.to_string(), h));
            }
            "expectfile" => {
                let (a, b) = (t.get(2).ok_or_else(bad)?, t.get(3).ok_or_else(bad)?);
                push(ms, ScriptAction::ExpectFile(a.to_string(), a.to_string(), base.join(b)));
            }
            "quit" => push(ms, ScriptAction::Quit),
            _ => return Err(bad()),
        }
    }
    out.sort_by_key(|e| e.ms);
    Ok(out)
}
