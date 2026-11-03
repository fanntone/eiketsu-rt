//! Remote control over localhost TCP, so another program (or an agent) can play a
//! running window: one command per connection, one reply line.
//!
//!   click X Y | rclick X Y | move X Y | key NAME | type TEXT   input (journaled like real input)
//!   shot FILE.png      save the current picture (640x480 coordinates = click coordinates)
//!   wait MS            reply after MS emulated milliseconds
//!   speed N            run emulated time at N x real time (windowed)
//!   status             emulated ms and current program
//!   tap X Y            click the center of map tile (X,Y) (battle map coordinates)
//!   dump FILE          write conventional memory (640 KiB) to FILE
//!   state              battle units as text (side, name, x, y, troops, level, EXP…)
//!
//! Client side: `eiketsu-rt ctl PORT COMMAND...`.

use crate::machine::Machine;
use crate::script::{ScriptAction, ScriptEvent};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

enum Reply {
    Now(String),
    After(u64),
}

pub struct Control {
    listener: TcpListener,
    /// Connections waiting for `wait`: (emulated ms deadline, stream).
    waiting: Vec<(u64, TcpStream)>,
}

impl Control {
    pub fn bind(port: u16) -> Result<Control, String> {
        let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("control port {port}: {e}"))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        Ok(Control { listener, waiting: Vec::new() })
    }
}

fn reply(mut s: TcpStream, msg: &str) {
    let _ = writeln!(s, "{msg}");
}

impl Machine {
    pub fn service_control(&mut self) {
        let Some(mut c) = self.control.take() else { return };
        let now = self.now_ms();
        let mut i = 0;
        while i < c.waiting.len() {
            if c.waiting[i].0 <= now {
                let (_, s) = c.waiting.swap_remove(i);
                reply(s, &format!("ok ms={now}"));
            } else {
                i += 1;
            }
        }
        while let Ok((s, _)) = c.listener.accept() {
            let _ = s.set_nonblocking(false);
            let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
            let mut line = String::new();
            if BufReader::new(&s).read_line(&mut line).is_err() {
                continue;
            }
            self.mem.epoch += 1;
            match self.command(line.trim()) {
                Ok(Reply::After(ms)) => c.waiting.push((now + ms, s)),
                Ok(Reply::Now(msg)) => reply(s, &msg),
                Err(e) => reply(s, &format!("error {e}")),
            }
        }
        self.control = Some(c);
    }

    fn command(&mut self, line: &str) -> Result<Reply, String> {
        let t: Vec<&str> = line.split_whitespace().collect();
        let now = self.now_ms();
        let num = |i: usize| t.get(i).and_then(|s| s.parse::<i64>().ok()).ok_or_else(|| format!("bad command: {line}"));
        match t.first().copied() {
            Some("click" | "rclick" | "move" | "key") => {
                let ev = crate::script::parse(&format!("0 {line}"), std::path::Path::new("."))?;
                self.schedule(now, ev);
            }
            Some("type") => {
                let text = line.splitn(2, ' ').nth(1).unwrap_or("");
                let mut evs = Vec::new();
                for (k, ch) in text.chars().enumerate() {
                    let name = if ch == ' ' { "SPACE".to_string() } else { ch.to_string() };
                    let mut e = crate::script::parse(&format!("{} key {name}", k * 150), std::path::Path::new("."))?;
                    evs.append(&mut e);
                }
                self.schedule(now, evs);
            }
            Some("shot") => {
                let p = t.get(1).ok_or("shot needs a file")?;
                std::fs::write(p, self.screenshot_png()).map_err(|e| e.to_string())?;
            }
            Some("wait") => return Ok(Reply::After(num(1)?.max(0) as u64)),
            Some("speed") => self.set_speed(num(1)?.clamp(1, 16) as u64),
            Some("tap") => {
                let (x, y) = (num(1)? as i32, num(2)? as i32);
                let (px, py) = crate::battle::tile_screen(&self.mem, x, y)
                    .ok_or_else(|| format!("tile ({x},{y}) is not in the map view; view = {:?} half tiles", crate::battle::view(&self.mem)))?;
                let ev = crate::script::parse(&format!("0 click {px} {py}"), std::path::Path::new("."))?;
                self.schedule(now, ev);
                return Ok(Reply::Now(format!("ok ms={now} clicked ({px},{py})")));
            }
            Some("rwatch") => {
                // rwatch ADDR(hex)… : start logging reads; "rwatch" alone: report and stop
                if t.len() > 1 {
                    self.mem.rwatch = t[1..].iter().filter_map(|s| u32::from_str_radix(s, 16).ok()).collect();
                    self.mem.rwatch_hits.clear();
                } else {
                    let mut seen = std::collections::BTreeMap::new();
                    for &(a, cs, ip) in &self.mem.rwatch_hits {
                        *seen.entry((a, cs, ip)).or_insert(0) += 1;
                    }
                    self.mem.rwatch.clear();
                    let lines: Vec<String> = seen.iter().map(|((a, cs, ip), n)| format!("{a:05x} by {cs:04x}:{ip:04x} x{n}")).collect();
                    return Ok(Reply::Now(format!("ok
{}", lines.join("
"))));
                }
            }
            Some("dump") => {
                let p = t.get(1).ok_or("dump needs a file")?;
                std::fs::write(p, &self.mem.ram[..0xa0000]).map_err(|e| e.to_string())?;
            }
            Some("state") => return Ok(Reply::Now(crate::battle::describe(&self.mem))),
            Some("status") => return Ok(Reply::Now(format!("ok ms={now} program={}", self.dos.current_name()))),
            _ => return Err(format!("unknown command: {line}")),
        }
        Ok(Reply::Now(format!("ok ms={now}")))
    }

    /// Queue scripted input relative to now (it is journaled when applied).
    pub fn schedule(&mut self, now: u64, evs: Vec<ScriptEvent>) {
        for mut e in evs {
            e.ms += now + 1;
            if !matches!(e.action, ScriptAction::Mouse(..) | ScriptAction::Key(..)) {
                continue;
            }
            self.cfg.script.push(e);
        }
        self.cfg.script.sort_by_key(|e| e.ms);
    }
}

/// `eiketsu-rt ctl PORT COMMAND...`: send one command, print the reply.
pub fn client(port: u16, cmd: &str) -> i32 {
    let mut s = match TcpStream::connect(("127.0.0.1", port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot reach the game on port {port}: {e}");
            return 1;
        }
    };
    if writeln!(s, "{cmd}").is_err() {
        return 1;
    }
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let out = out.trim();
    println!("{out}");
    if out.starts_with("ok") { 0 } else { 1 }
}
