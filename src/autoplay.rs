//! Autoplay: plays 汜水關 by itself, headless. Every decision is made from the
//! game's memory (`battle::units`); each candidate move is tried on a snapshot and
//! rolled back, and the best one is then played for real. Strategy knobs come from
//! `Params`, so many copies can run in parallel with different settings.
//!
//! The only screen access is a pixel check that the action menu opened.

use crate::battle::{self, Unit};
use crate::machine::{Config, Machine, Stop};
use crate::IPS_PER_MS;
use std::collections::HashSet;
use std::path::PathBuf;

/// Input that takes the save in slot 2 into the battle (talk only to 關羽).
const BOOT: &str = "9000 key A\n9400 key ENTER\n14000 click 320 211\n16500 click 220 200\n18000 click 350 231\n\
25000 click 222 238\n26500 click 222 238\n29000 click 382 152\n32000 click 295 231\n";
/// 汜水關's tiles that restore troops: the 鹿砦 and the two villages (default of `heal`).
const HEAL_TILES: [(u8, u8); 3] = [(11, 8), (13, 12), (19, 9)];
const TENTS: [(u8, u8); 2] = [(6, 7), (12, 6)];
/// The only tile next to 華雄.
const HUAXIONG_GATE: (u8, u8) = (4, 9);
/// The 鹿砦 in the middle of the map (heals whoever stands on it): 胡軫 is lured
/// onto it and farmed there.
const LUJAI: (u8, u8) = (11, 8);
/// Generals farmed on the 鹿砦: beaten low they flee onto it and heal every turn.
const FARM: [&str; 2] = ["趙岑", "胡軫"];

#[derive(Clone, Debug)]
pub struct Params {
    /// No kills of normal enemies before this turn.
    pub kill_turn: u32,
    /// Turn on which 關羽 attacks 華雄 (duel ends the battle).
    pub final_turn: u32,
    /// Units below this many troops go to a healing tile.
    pub heal_below: u16,
    /// Score penalty per point of damage dealt (prefers weak hits → longer-lived enemies).
    pub damage_weight: f32,
    /// Bonus for EXP going to each unit (劉備, 關羽, 張飛).
    /// EXP weight per general index (1.0 when not set; w=G:V sets any general).
    pub exp_weight: Vec<f32>,
    /// Max Manhattan distance for candidate destinations.
    pub reach: i32,
    /// Stop after this many turns without the duel (for stepwise testing).
    pub stop_turn: u32,
    /// Risk model: damage per enemy hit, how many enemies can hit in one phase,
    /// and the score weight of the expected damage fraction.
    pub hit_dmg: f32,
    pub max_hits: f32,
    pub risk_weight: f32,
    /// A turn whose enemy phase leaves anyone at or below this many troops is replayed.
    pub floor: u16,
    /// Score per point of damage dealt to 華雄 (softening him for 劉備).
    pub hua_weight: f32,
    /// 劉備 may kill 華雄 before the final turn.
    pub hua_early: bool,
    /// Final turn: 劉備 last-hits 華雄 if a trial shows he can (else 關羽's duel).
    pub liu_finale: bool,
    /// Once 李肅 and 趙岑 are gone, lure 胡軫 onto the 鹿砦 and farm him there.
    pub lure: bool,
    /// Score per point of damage on 趙岑 while he is off the 鹿砦.
    pub zhao_weight: f32,
    /// From this turn on, beat the farmed generals low (before it they are milked).
    pub zhao_push_turn: u32,
    /// 趙岑 may be finished from this turn on (then 胡軫 is lured onto the 鹿砦).
    pub zhao_kill_turn: u32,
    /// While farming 胡軫: units with fewer troops heal instead of attacking.
    pub farm_min: u16,
    /// While farming 胡軫: at most this many plans are tried per turn.
    pub farm_trials: u32,
    /// While farming 胡軫: below this many troops he is left to heal (no attacks).
    pub farm_hu_min: u16,
    /// While farming 胡軫: plans leaving him at this many troops or more are
    /// penalised (at full strength he walks off the 鹿砦).
    pub farm_hu_max: u16,
    /// Endgame: below this many troops 劉備 / 張飛 heal on the 鹿砦 instead of
    /// lining up for 華雄.
    pub hua_min: u16,
    /// Keep the 鹿砦 as our own healing station: 胡軫 is drawn out of the gate
    /// and fought on open ground instead of being farmed on it.
    pub lujai_ours: bool,
    /// Tiles that restore troops (villages, the 鹿砦); heal=x,y;x,y.
    pub heal: Vec<(u8, u8)>,
    /// After resuming, click here once (the opening 勝利條件 box's 確認).
    pub intro_click: Option<(i32, i32)>,
    /// Use the general turn planner (gplan_turn) on every turn.
    pub gplan: bool,
    /// Final turn on maps other than 汜水關: this unit attacks the boss.
    pub finale_g: u16,
    /// Planner: units other than finale_g may attack the boss (while he has at
    /// least boss_hit_min troops).
    pub boss_hit: bool,
    /// Planner: keep at least this many non-boss enemies alive before kill_turn.
    pub keep_alive: u16,
    /// 關羽 / 張飛 feed 豆 to allies below feed_below troops instead of idling.
    pub feed: bool,
    /// Weak units eat one of their own 豆 (below eat_below troops) instead of walking to a village.
    pub eat: bool,
    pub eat_below: u16,
    pub feed_below: u16,
    /// Who feeds (feeders=1,2 by default).
    pub feeders: Vec<u16>,
    /// Turn from which the boss acts on his own (虎牢關: 呂布 on turn 18); before it,
    /// a lone boss is farmed instead of finishing early.
    pub boss_wake: u32,
    /// Planner: unit `g` first fetches the item on tile (x,y) (pickup=g@x,y).
    pub pickup: Option<(u16, (u8, u8))>,
    /// Planner: tiles never to stop on (e.g. tents whose item cannot be carried:
    /// the game then asks to throw something away).
    pub avoid: Vec<(u8, u8)>,
    pub boss_hit_min: u16,
    /// ... and only by units with at least this many troops.
    pub boss_hit_troops: u16,
    /// Planner: idle units with at least this many troops advance instead of healing.
    pub advance_min: u16,
    /// The general whose fall ends the battle (kept alive before kill_turn).
    pub boss: String,
    /// Score for standing where exactly one enemy can reach (lures it out alone).
    pub lure_bonus: f32,
    /// An enemy at or below this many troops dies to one more hit anyway.
    pub one_hit: u16,
    /// Continue from this save state (written as state_T<n>.bin after every turn).
    pub resume: Option<PathBuf>,
    /// Open a native window and watch the AI play in real time (at `speed`x).
    pub live: bool,
    pub speed: u64,
    pub clear_journal: bool,
}

impl Default for Params {
    fn default() -> Params {
        Params { kill_turn: 26, final_turn: 30, heal_below: 260, damage_weight: 0.02, exp_weight: vec![1.0; 3], reach: 8, stop_turn: 999, hit_dmg: 130.0, max_hits: 2.0, risk_weight: 30.0, floor: 100, hua_weight: 0.0, hua_early: false, liu_finale: false, lure: true, zhao_weight: 0.1, zhao_push_turn: 12, zhao_kill_turn: 18, farm_min: 250, farm_trials: 12, farm_hu_min: 120, farm_hu_max: 520, hua_min: 450, lujai_ours: false, heal: HEAL_TILES.to_vec(), intro_click: None, gplan: false, finale_g: 2, boss_hit: false, keep_alive: 0, feed: false, eat: false, eat_below: 300, feed_below: 300, feeders: vec![1, 2], boss_wake: 0, pickup: None, avoid: Vec::new(), boss_hit_min: 400, boss_hit_troops: 520, advance_min: 450, boss: "華雄".into(), lure_bonus: 20.0, one_hit: 300, resume: None, live: false, speed: 3, clear_journal: false }
    }
}

impl Params {
    pub fn set(&mut self, kv: &str) -> Result<(), String> {
        let (k, v) = kv.split_once('=').ok_or_else(|| format!("bad param {kv}"))?;
        let f = |v: &str| v.parse::<f32>().map_err(|_| format!("bad value {kv}"));
        match k {
            "kill_turn" => self.kill_turn = f(v)? as u32,
            "final_turn" => self.final_turn = f(v)? as u32,
            "heal_below" => self.heal_below = f(v)? as u16,
            "damage_weight" => self.damage_weight = f(v)?,
            "reach" => self.reach = f(v)? as i32,
            "stop_turn" => self.stop_turn = f(v)? as u32,
            "hit_dmg" => self.hit_dmg = f(v)?,
            "max_hits" => self.max_hits = f(v)?,
            "risk_weight" => self.risk_weight = f(v)?,
            "floor" => self.floor = f(v)? as u16,
            "hua_weight" => self.hua_weight = f(v)? as f32,
            "hua_early" => self.hua_early = f(v)? != 0.0,
            "liu_finale" => self.liu_finale = f(v)? != 0.0,
            "lure" => self.lure = f(v)? != 0.0,
            "zhao_weight" => self.zhao_weight = f(v)?,
            "zhao_push_turn" => self.zhao_push_turn = f(v)? as u32,
            "zhao_kill_turn" => self.zhao_kill_turn = f(v)? as u32,
            "farm_min" => self.farm_min = f(v)? as u16,
            "farm_trials" => self.farm_trials = f(v)? as u32,
            "farm_hu_min" => self.farm_hu_min = f(v)? as u16,
            "farm_hu_max" => self.farm_hu_max = f(v)? as u16,
            "hua_min" => self.hua_min = f(v)? as u16,
            "lujai_ours" => self.lujai_ours = f(v)? != 0.0,
            "gplan" => self.gplan = f(v)? != 0.0,
            "finale_g" => self.finale_g = f(v)? as u16,
            "boss_hit" => self.boss_hit = f(v)? != 0.0,
            "keep_alive" => self.keep_alive = f(v)? as u16,
            "feed" => self.feed = f(v)? != 0.0,
            "eat" => self.eat = f(v)? != 0.0,
            "eat_below" => self.eat_below = f(v)? as u16,
            "feed_below" => self.feed_below = f(v)? as u16,
            "feeders" => self.feeders = v.split(',').map(|g| g.trim().parse().map_err(|_| "feeders=g,g")).collect::<Result<_, _>>()?,
            "boss_wake" => self.boss_wake = f(v)? as u32,
            "pickup" => {
                let (g, t) = v.split_once('@').ok_or("pickup=g@x,y")?;
                let (x, y) = t.split_once(',').ok_or("pickup=g@x,y")?;
                self.pickup = Some((g.parse().map_err(|_| "pickup g")?, (x.parse().map_err(|_| "pickup x")?, y.parse().map_err(|_| "pickup y")?)));
            }
            "avoid" => {
                self.avoid = v.split(';').map(|t| {
                    let (a, b) = t.split_once(',').ok_or("avoid=x,y;x,y")?;
                    Ok((a.trim().parse().map_err(|_| "avoid x")?, b.trim().parse().map_err(|_| "avoid y")?))
                }).collect::<Result<_, &str>>()?;
            }
            "boss_hit_min" => self.boss_hit_min = f(v)? as u16,
            "boss_hit_troops" => self.boss_hit_troops = f(v)? as u16,
            "advance_min" => self.advance_min = f(v)? as u16,
            "boss" => self.boss = match v {
                "lubu" | "lvbu" => "呂布".into(),
                "huaxiong" => "華雄".into(),
                _ => v.to_string(),
            },
            "intro_click" => {
                let (a, b) = v.split_once(',').ok_or("intro_click=x,y")?;
                self.intro_click = Some((a.parse().map_err(|_| "intro_click x")?, b.parse().map_err(|_| "intro_click y")?));
            }
            "heal" => {
                self.heal = v.split(';').map(|t| {
                    let (a, b) = t.split_once(',').ok_or_else(|| format!("bad tile {t}"))?;
                    Ok((a.trim().parse().map_err(|_| format!("bad tile {t}"))?, b.trim().parse().map_err(|_| format!("bad tile {t}"))?))
                }).collect::<Result<_, String>>()?;
            }
            "lure_bonus" => self.lure_bonus = f(v)?,
            "one_hit" => self.one_hit = f(v)? as u16,
            "resume" => self.resume = Some(PathBuf::from(v)),
            "live" => self.live = f(v)? != 0.0,
            "speed" => self.speed = (f(v)? as u64).max(1),
            "clear_journal" => self.clear_journal = f(v)? != 0.0,
            "w" => {
                let (g, w) = v.split_once(':').ok_or("w=G:V")?;
                let g: usize = g.parse().map_err(|_| "w general")?;
                if self.exp_weight.len() <= g { self.exp_weight.resize(g + 1, 1.0); }
                self.exp_weight[g] = f(w)?;
            }
            "w_liu" => self.exp_weight[0] = f(v)?,
            "w_guan" => self.exp_weight[1] = f(v)?,
            "w_zhang" => self.exp_weight[2] = f(v)?,
            _ => return Err(format!("unknown param {k}")),
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Act {
    Attack(u8, u8),
    /// Use the item in this inventory slot (a fire book: 焦熱書) on the target.
    Fire(u8, u8, u8),
    Rest,
}

/// Item code of 焦熱書 (fire attack; adjacent target; not in rain).
const FIRE_BOOK: u8 = 0x28;
/// Weather byte, general table + 0x6753: 0..2 = clear-sky icon, 3+ = rain (no fire).
const WEATHER: u32 = 0x6753;

pub struct Bot {
    pub m: Machine,
    pub p: Params,
    pub turn: u32,
    pub log: Vec<String>,
    tents_left: Vec<(u8, u8)>,
    /// 0 = normal, 1 = cautious (stricter risk), 2 = retreat only (no attacks).
    caution: u32,
    /// Final turn: 劉備 last-hits 華雄 from the gate (instead of 關羽's duel).
    finale_kill: bool,
    /// Items the pickup unit held before going for its item (None = not started).
    pickup_items: Option<usize>,
    /// The battle ended during a turn (the boss's duel started in the enemy phase).
    battle_over: bool,
    /// Units on our side at the start of the current turn.
    n_ours: usize,
    /// Farm turn in progress: 胡軫's tile and the healing tiles to use.
    farm_at: (u8, u8),
    farm_heal: Vec<(u8, u8)>,
    start_exp: i32,
    /// Per turn: for every enemy, the tiles it can move to (read from the game).
    enemy_ranges: Vec<((u8, u8), HashSet<(u8, u8)>)>,
    out: PathBuf,
    last_click: Option<(i32, i32)>,
    dismiss_toggle: bool,
    /// Movement range of the currently selected unit (from memory).
    last_reach: HashSet<(u8, u8)>,
}

fn total_exp(u: &Unit) -> i32 {
    u.level as i32 * 100 + u.exp as i32
}

fn dist(a: (u8, u8), b: (u8, u8)) -> i32 {
    (a.0 as i32 - b.0 as i32).abs() + (a.1 as i32 - b.1 as i32).abs()
}

/// Healing tiles used while farming 胡軫: the village below and the village east
/// (the tent (12,6) restores no troops).
const FARM_HEAL: [(u8, u8); 2] = [(13, 12), (19, 9)];

/// Every ordering of `items`.
fn order_all(items: &[u16], cur: &mut Vec<u16>, out: &mut Vec<Vec<u16>>) {
    if cur.len() == items.len() {
        out.push(cur.clone());
        return;
    }
    for &g in items {
        if !cur.contains(&g) {
            cur.push(g);
            order_all(items, cur, out);
            cur.pop();
        }
    }
}

/// All choices of `k` units from `items` (in order of `items`).
fn choose(items: &[u16], k: usize, cur: &mut Vec<u16>, out: &mut Vec<Vec<u16>>) {
    if cur.len() == k {
        out.push(cur.clone());
        return;
    }
    let start = cur.last().and_then(|l| items.iter().position(|x| x == l)).map_or(0, |i| i + 1);
    for &g in &items[start..] {
        cur.push(g);
        choose(items, k, cur, out);
        cur.pop();
    }
}

/// All ordered choices of `k` items from `items`.
fn permute(items: &[(u8, u8)], k: usize, cur: &mut Vec<(u8, u8)>, out: &mut Vec<Vec<(u8, u8)>>) {
    if cur.len() == k {
        out.push(cur.clone());
        return;
    }
    for &t in items {
        if !cur.contains(&t) {
            cur.push(t);
            permute(items, k, cur, out);
            cur.pop();
        }
    }
}

fn neighbours(t: (u8, u8)) -> Vec<(u8, u8)> {
    let (x, y) = (t.0 as i32, t.1 as i32);
    [(x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)].into_iter()
        .filter(|&(a, b)| a >= 0 && b >= 0)
        .map(|(a, b)| (a as u8, b as u8))
        .collect()
}

impl Bot {
    pub fn new(game_dir: PathBuf, out: PathBuf, p: Params) -> Result<Bot, String> {
        let cfg = Config {
            game_dir,
            headless: !p.live,
            trace: false,
            journal: None,
            battle_log: None,
            debug_grid: p.live,
            replay: None,
            max_steps: Some(0),
            script: crate::script::parse(BOOT, std::path::Path::new("."))?,
            program: r"C:\REKO3.COM".into(),
            audio: p.live,
            window: None,
            control: None,
            speed: if p.live { p.speed.max(1) } else { 1 },
        };
        let mut m = Machine::new(cfg)?;
        m.bot_mode = true;
        std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
        Ok(Bot { m, p, turn: 1, log: Vec::new(), tents_left: TENTS.to_vec(), caution: 0, finale_kill: false, pickup_items: None, battle_over: false, n_ours: 3, farm_at: LUJAI, farm_heal: Vec::new(), start_exp: 0, enemy_ranges: Vec::new(), out, last_click: None, dismiss_toggle: false, last_reach: HashSet::new() })
    }

    fn note(&mut self, s: String) {
        eprintln!("[bot] {s}");
        // Also appended to log.txt right away, so a running bot can be watched.
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.out.join("log.txt")) {
            use std::io::Write;
            let _ = writeln!(f, "{s}");
        }
        self.log.push(s);
    }

    fn run_ms(&mut self, ms: u64) -> Result<(), String> {
        self.m.cfg.max_steps = Some(self.m.cpu.steps + ms * IPS_PER_MS);
        match self.m.run() {
            Stop::Budget => Ok(()),
            other => Err(format!("emulation stopped: {other:?}")),
        }
    }

    fn input(&mut self, line: &str) -> Result<(), String> {
        let ev = crate::script::parse(&format!("0 {line}"), std::path::Path::new("."))?;
        let now = self.m.now_ms();
        self.m.schedule(now, ev);
        Ok(())
    }

    fn click(&mut self, x: i32, y: i32, after_ms: u64) -> Result<(), String> {
        self.input(&format!("click {x} {y}"))?;
        self.last_click = Some((x, y));
        // The click releases 150 ms after it starts.
        self.run_ms(after_ms.max(200))
    }

    fn tap(&mut self, t: (u8, u8), after_ms: u64) -> Result<(), String> {
        let (x, y) = battle::tile_screen(&self.m.mem, t.0 as i32, t.1 as i32).ok_or_else(|| format!("tile {t:?} not in view"))?;
        if std::env::var("EIKETSU_BOT_DEBUG").is_ok() {
            eprintln!("[tap] {t:?} view={:?} -> ({x},{y})", battle::view(&self.m.mem));
        }
        self.click(x, y, after_ms)
    }

    /// Run until the picture stops changing (animations, enemy moves).
    fn settle(&mut self, max_ms: u64) -> Result<(), String> {
        let mut last = self.m.screen_hash();
        let mut same = 0;
        let mut t = 0;
        while t < max_ms {
            self.run_ms(100)?;
            t += 100;
            let h = self.m.screen_hash();
            if h == last {
                same += 1;
                if same >= 2 {
                    return Ok(());
                }
            } else {
                same = 0;
                last = h;
            }
        }
        Ok(())
    }

    /// Run in 50 ms slices until `f` holds; false on timeout.
    fn wait_until(&mut self, f: impl Fn(&Bot) -> bool, max_ms: u64) -> Result<bool, String> {
        let mut t = 0;
        while t < max_ms {
            if f(self) {
                return Ok(true);
            }
            self.run_ms(50)?;
            t += 50;
        }
        Ok(f(self))
    }

    /// Find the open action menu (攻擊/策略/道具/休息) on the map: a 112 px tall
    /// black frame, 79 px wide. Returns its top-left corner.
    fn find_menu(&self) -> Option<(i32, i32)> {
        let (w, _, px) = self.m.frame();
        let black = |x: i32, y: i32| px[y as usize * w + x as usize] == 0;
        for x in 16..(432 - 79) {
            let mut y = 72;
            while y <= 424 - 112 {
                if black(x, y) {
                    let mut run = 0;
                    while y + run < 424 && black(x, y + run) {
                        run += 1;
                    }
                    // One side may be partly covered by the mouse cursor: accept a full
                    // 112 px edge whose partner 79 px away is at least half visible.
                    let partner = |px: i32| (0..112).filter(|&k| black(px, y + k)).count() >= 56;
                    if run >= 112 && x + 79 < 432 && partner(x + 79) {
                        return Some((x, y));
                    }
                    if run >= 112 && x - 79 >= 16 && partner(x - 79) {
                        return Some((x - 79, y));
                    }
                    y += run;
                } else {
                    y += 1;
                }
            }
        }
        None
    }

    /// Range shown (general table + 0x678B): 1 while the movement range is up,
    /// 0 once the action menu replaced it.
    fn range_shown(&self) -> bool {
        battle::general_table(&self.m.mem).map_or(false, |gt| self.m.mem.ram[(gt + 0x678b) as usize] == 1)
    }

    /// The action menu (攻擊/策略/道具/休息) is open.
    fn action_menu_open(&self) -> bool {
        // After the destination click the range overlay is replaced by the menu.
        self.unit_selected() && !self.range_shown()
    }

    /// The 功能 menu (top-left) is open: its black frame runs ~220 px down at x≈52.
    fn func_menu_open(&self) -> bool {
        // Its black frame in video memory: a 200 px vertical line at x≈48.
        let (w, _, px) = self.m.frame();
        let black = |x: i32, y: i32| px[y as usize * w + x as usize] == 0;
        (46..53).any(|x| (84..100).any(|y0| (0..200).all(|k| black(x, y0 + k))))
    }

    /// A yes/no dialog (e.g. 「結束所有部隊的命令嗎？」) is open: black frame lines
    /// at y=184 and y=260 spanning x 112..336. Buttons: 是 (222,232), 否 (280,232).
    fn dialog_open(&self) -> bool {
        // Its black frame in video memory: horizontal lines at y=184 and y=260.
        let (w, _, px) = self.m.frame();
        let black = |x: i32, y: i32| px[y as usize * w + x as usize] == 0;
        (120..320).all(|x| black(x, 184)) && (120..320).all(|x| black(x, 260))
    }

    /// A unit is selected (its movement range is being shown): the game's range
    /// buffer, 0x31D21 past the general table, is non-zero then and cleared otherwise.
    fn unit_selected(&self) -> bool {
        match battle::general_table(&self.m.mem) {
            Some(gt) => self.m.mem.ram[(gt + 0x31d21) as usize..(gt + 0x31d21 + 0x23c) as usize].iter().any(|&b| b != 0),
            None => false,
        }
    }

    /// Position of the selected unit (general table + 0x83D1: x, y).
    fn selected_pos(&self) -> Option<(u8, u8)> {
        let gt = battle::general_table(&self.m.mem)? as usize;
        Some((self.m.mem.ram[gt + 0x83d1], self.m.mem.ram[gt + 0x83d2]))
    }

    /// Screen point of a visible map tile with no unit on it (for cancelling clicks).
    fn empty_spot(&self) -> (i32, i32) {
        let units = self.units();
        if let Some((sx, sy)) = battle::view(&self.m.mem) {
            let (bx, by) = ((sx + 1) / 2, (sy + 1) / 2);
            for ty in (by..by + 11).rev() {
                for tx in (bx..bx + 13).rev() {
                    if units.iter().any(|u| (u.x as i32, u.y as i32) == (tx, ty)) {
                        continue;
                    }
                    if let Some(p) = battle::tile_screen(&self.m.mem, tx, ty) {
                        return p;
                    }
                }
            }
        }
        (400, 400)
    }

    /// Bring the game back to the idle map: no action menu, no 功能 menu, no unit
    /// selected, picture stable. Verified after every correction.
    fn ensure_idle(&mut self) -> Result<(), String> {
        for _ in 0..8 {
            self.settle(1500)?;
            if self.dialog_open() {
                if self.all_acted() {
                    return Ok(()); // the end-turn prompt; end_turn() answers it
                }
                self.click(280, 232, 500)?; // 否
                continue;
            }
            if self.unit_selected() || self.func_menu_open() {
                let (x, y) = self.empty_spot();
                self.input(&format!("rclick {x} {y}"))?;
                self.run_ms(500)?;
                continue;
            }
            return Ok(());
        }
        let _ = self.shot("dbg_not_idle");
        Err("could not return to the idle map (see dbg_not_idle.png)".into())
    }

    /// Dismiss whatever message is showing (left click = confirm / next; right
    /// click means "back" in this game and is only used to cancel a selection).
    fn dismiss(&mut self) -> Result<(), String> {
        // Below the map (bottom frame): a click on the map itself could select or
        // move a unit standing there when no message is actually up.
        let (x, y) = std::env::var("EIKETSU_BOT_DISMISS").ok()
            .and_then(|v| { let mut it = v.split(',').map(|n| n.trim().parse().ok()); Some((it.next()??, it.next()??)) })
            .unwrap_or((240, 445));
        self.click(x, y, 100)
    }

    fn pixel(&self, x: i32, y: i32) -> u32 {
        let (w, _, px) = self.m.frame();
        px[y as usize * w + x as usize]
    }

    pub fn units(&self) -> Vec<Unit> {
        battle::units(&self.m.mem).unwrap_or_default()
    }

    fn ours(&self) -> Vec<Unit> {
        self.units().into_iter().filter(|u| u.side == "我軍" && (u.general < 3 || self.p.gplan)).collect()
    }

    fn enemies(&self) -> Vec<Unit> {
        self.units().into_iter().filter(|u| u.side == "敵軍" && u.troops > 0).collect()
    }

    pub fn boot(&mut self) -> Result<(), String> {
        self.run_ms(35_000)?;
        for _ in 0..20 {
            if self.units().iter().any(|u| u.side == "敵軍") && self.m.now_ms() > 60_000 {
                break;
            }
            self.click(320, 300, 2500)?;
        }
        for _ in 0..8 {
            self.click(320, 300, 2500)?;
        }
        self.set_battle_options()?;
        if std::env::var("EIKETSU_BOT_DEBUG").is_ok() {
            let _ = self.shot("dbg_after_options");
        }
        self.note(format!("options byte = {:02x} (animation off, speed 5)", self.options().unwrap_or(0)));
        self.shot("start")?;
        self.note(format!("battle ready at {} ms", self.m.now_ms()));
        Ok(())
    }

    fn set_battle_options(&mut self) -> Result<(), String> {
        // Battle animations off (功能 → 部隊戰鬥 → 不觀看) and movement speed 5
        // (功能 → 移動速度 → 5速). The option byte sits 0x15 before the general
        // table: bit 0x10 = 不觀看, low nibble = speed - 1. Verified from memory,
        // retried until it took (the clicks are journaled, so a replay matches).
        for attempt in 0..6 {
            // Animations off is what matters; a speed the player chose is kept.
            if self.options().map_or(false, |o| o == 0x14 || (attempt == 0 && o & 0x10 != 0)) {
                break;
            }
            if attempt == 5 {
                return Err(format!("could not set battle options (byte = {:?})", self.options()));
            }
            self.settle(2000)?;
            self.click(45, 55, 900)?;
            self.click(85, 184, 900)?;
            self.click(135, 205, 1200)?;
            self.settle(1500)?;
            self.click(45, 55, 900)?;
            self.click(85, 208, 900)?;
            self.click(135, 256, 1200)?;
            // Choosing a submenu entry leaves the 功能 menu open; a right click on
            // an empty map spot closes it.
            self.input("rclick 400 400")?;
            self.run_ms(800)?;
            self.settle(2000)?;
        }
        Ok(())
    }

    /// The battle option byte (bit 0x10 = animations off, low nibble = speed - 1).
    fn options(&self) -> Option<u8> {
        battle::general_table(&self.m.mem).map(|gt| self.m.mem.ram[(gt - 0x15) as usize])
    }

    fn shot(&self, name: &str) -> Result<(), String> {
        std::fs::write(self.out.join(format!("{name}.png")), self.m.screenshot_png()).map_err(|e| e.to_string())?;
        self.grid_shot(name)
    }

    /// The same picture with the tile grid laid over it (see battle::draw_grid).
    fn grid_shot(&self, name: &str) -> Result<(), String> {
        let (w, h, mut px) = self.m.frame();
        battle::draw_grid(&self.m.mem, w, h, &mut px);
        let rgb: Vec<u8> = px.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect();
        std::fs::write(self.out.join(format!("{name}_grid.png")), crate::png::encode_rgb(w, h, &rgb)).map_err(|e| e.to_string())
    }

    /// Select unit `g` through the 我軍 list and read its movement range off the
    /// picture: the game dims every unreachable tile, so the tiles that did not
    /// change between the frame before and after selection are the range. Water
    /// animation is excluded by comparing two pre-selection frames. Leaves the unit
    /// selected.
    fn select(&mut self, g: u16) -> Result<HashSet<(u8, u8)>, String> {
        let u = self.units().into_iter().find(|u| u.general == g).ok_or("unit gone")?;
        if u.acted {
            return Err("already acted".into());
        }
        // The 我軍 list shows our units in unit-table order (劉備/關羽/張飛 first).
        let row = self.units().iter().filter(|o| o.side == "我軍").position(|o| o.general == g).unwrap_or(g as usize) as i32;
        let r = self.read_range(545, 122 + 19 * row, (u.x, u.y), &format!("unit {g}"))?;
        self.last_reach = r.clone();
        Ok(r)
    }

    /// Movement range of an enemy (its row in the 敵軍 list), read the same way.
    /// Leaves nothing selected.
    fn enemy_reach(&mut self, row: i32, at: (u8, u8)) -> Result<HashSet<(u8, u8)>, String> {
        let r = self.read_range(575, 122 + 19 * row, at, &format!("enemy row {row}"))?;
        self.clear_overlay()?;
        Ok(r)
    }

    /// Cancel a shown range (ours or an enemy's) and wait until the map is drawn
    /// normally again: the next range read compares against this picture.
    fn clear_overlay(&mut self) -> Result<(), String> {
        for _ in 0..2 {
            let (x, y) = self.empty_spot();
            self.input(&format!("rclick {x} {y}"))?;
            self.run_ms(400)?;
        }
        self.click(545, 88, 300)?; // back to the 我軍 list
        self.ensure_idle()?;
        self.settle(1500)
    }

    /// Select a unit through a list tab (我軍 545 / 敵軍 575) and row, and read
    /// its movement range off the picture. Leaves the unit selected.
    fn read_range(&mut self, tab_x: i32, row_y: i32, at: (u8, u8), what: &str) -> Result<HashSet<(u8, u8)>, String> {
        self.ensure_idle()?;
        let mut ok = false;
        for _ in 0..3 {
            self.click(tab_x, 88, 300)?;
            self.click(470, row_y, 300)?;
            // The overlay can still hold a range from the enemy phase: wait until
            // the game reports this very unit as the selected one.
            if self.wait_until(|b: &Bot| b.unit_selected() && b.selected_pos() == Some(at), 2000)? {
                ok = true;
                break;
            }
            self.ensure_idle()?;
        }
        if !ok {
            let _ = self.shot(&format!("dbg_select_failed_{}", what.replace(' ', "_")));
            return Err(format!("could not select {what} (range_shown={} selected={} func_menu={})", self.range_shown(), self.unit_selected(), self.func_menu_open()));
        }
        // Input is ignored while the map scrolls and the range is drawn.
        self.settle(2500)?;
        if let Ok(d) = std::env::var("EIKETSU_BOT_SELDUMP") {
            let f = format!("{d}/sel_{}_{}.bin", at.0, at.1);
            std::fs::write(&f, &self.m.mem.ram).map_err(|e| e.to_string())?;
        }
        let reach = self.range_from_memory(at);
        if std::env::var("EIKETSU_BOT_DEBUG").is_ok() {
            let mut r: Vec<_> = reach.iter().copied().collect();
            r.sort();
            eprintln!("[reach] {what} {:?}", r);
        }
        Ok(reach)
    }

    /// The game's range overlay: 26x22 cells (2x2 per tile) over the current map
    /// view, at general table + 0x31D21; 0 = reachable, non-zero = dimmed.
    fn range_from_memory(&self, at: (u8, u8)) -> HashSet<(u8, u8)> {
        let mut out = HashSet::new();
        out.insert(at);
        let (Some(gt), Some((vx, vy))) = (battle::general_table(&self.m.mem), battle::view(&self.m.mem)) else { return out };
        let base = (gt + 0x31d21) as usize;
        for cy in 0..22i32 {
            for cx in 0..26i32 {
                let (hx, hy) = (vx + cx, vy + cy);
                if hx % 2 != 0 || hy % 2 != 0 {
                    continue;
                }
                if self.m.mem.ram[base + (cy * 26 + cx) as usize] == 0 {
                    out.insert(((hx / 2) as u8, (hy / 2) as u8));
                }
            }
        }
        out
    }

    /// Map tile under a screen point for a given view (half-tile units).
    fn screen_tile(view: (i32, i32), px: i32, py: i32) -> Option<(u8, u8)> {
        if px < 16 || px >= 432 || py < 72 || py >= 424 {
            return None;
        }
        let x = (px - 16) / 32 + (view.0 + 1) / 2;
        let y = (py - 72) / 32 + (view.1 + 1) / 2;
        Some((x as u8, y as u8))
    }


    /// With the unit selected: move to `dest`, then attack / rest.
    fn commit(&mut self, g: u16, dest: (u8, u8), act: Act) -> Result<(), String> {
        if !self.last_reach.contains(&dest) {
            return Err(format!("{dest:?} is outside the movement range"));
        }
        // Same input sequence every time this is replayed from a snapshot.
        self.dismiss_toggle = false;
        if !self.unit_selected() {
            return Err("no unit selected".into());
        }
        // The range was read from memory, so a destination inside it always opens
        // the action menu. Opening it clears the range buffer (memory), which tells
        // us the click was taken; otherwise click the tile again.
        let mut opened = false;
        for _ in 0..3 {
            self.tap(dest, 300)?;
            if self.wait_until(|b: &Bot| !b.unit_selected(), 1500)? {
                opened = true;
                break;
            }
        }
        if !opened {
            return Err(format!("menu did not open at {dest:?}"));
        }
        self.run_ms(300)?;
        // Menu geometry (measured): 79x112 frame, centre 57 px left of the tile when
        // the tile is right of x=224 (else right of it), top 16 px above the tile
        // centre, kept inside the map area; four 24 px rows, labels at top+19/43/67/91.
        let (px, py0) = battle::tile_screen(&self.m.mem, dest.0 as i32, dest.1 as i32).ok_or("dest not in view")?;
        let mx = if px >= 224 { px - 57 } else { px + 57 };
        let top = (py0 - 16).clamp(72, 424 - 112);
        let row = match act { Act::Rest => top + 91, Act::Fire(..) => top + 67, Act::Attack(..) => top + 19 };
        let items_before: Vec<Vec<u8>> = self.ours().iter().map(|u| u.items.clone()).collect();
        let my_exp = |b: &Bot| b.units().iter().find(|u| u.general == g).map(|u| total_exp(u)).unwrap_or(0);
        let all_troops = |b: &Bot| -> Vec<(u16, u16)> { b.units().into_iter().map(|u| (u.general, u.troops)).collect() };
        let (exp0, troops0) = (my_exp(self), all_troops(self));
        self.click(mx, row, 400)?;
        if let Act::Fire(_, _, slot) = act {
            // 道具 → 使用 (22 px left, top+35) → the item list, whose last row sits
            // at y=404 whatever the menu position (rows 20 px apart).
            let n = self.units().into_iter().find(|u| u.general == g).map_or(0, |u| u.items.len()) as i32;
            self.settle(800)?;
            self.click(mx - 22, top + 35, 400)?;
            self.settle(800)?;
            self.click(mx - 22, 404 - 20 * (n - 1 - slot as i32), 400)?;
        }
        match act {
            Act::Attack(tx, ty) | Act::Fire(tx, ty, _) => {
                // (the 攻擊 row was clicked above) The target cursor appears once the menu has closed and the
                // attackable tiles are marked.
                self.settle(1500)?;
                if std::env::var("EIKETSU_BOT_DEBUG").is_ok() {
                    let _ = self.shot("dbg_after_attack_menu");
                }
                // A target on the view's edge row makes the game scroll on the first
                // click instead of attacking: tap again from the new view.
                for _ in 0..3 {
                    let v0 = battle::view(&self.m.mem);
                    self.tap((tx, ty), 300)?;
                    self.run_ms(500)?;
                    if battle::view(&self.m.mem) == v0 {
                        break;
                    }
                    self.settle(1500)?;
                }
                // An item's target cursor may want a second click to confirm.
                if matches!(act, Act::Fire(..)) {
                    self.run_ms(700)?;
                    if self.unit_selected() {
                        self.tap((tx, ty), 300)?;
                    }
                }
                if std::env::var("EIKETSU_BOT_DEBUG").is_ok() {
                    eprintln!("[tap-target] {:?} at {:?} view {:?}", (tx, ty), battle::tile_screen(&self.m.mem, tx as i32, ty as i32), battle::view(&self.m.mem));
                    let _ = self.shot("dbg_after_target_tap");
                    self.run_ms(1000)?;
                    let _ = self.shot("dbg_after_target_tap_1s");
                }
            }
            Act::Rest => {}
        }
        // A battle shows a result screen (EXP message) that waits for a click;
        // EXP and troops are written once it is dismissed, so wait for that.
        let mut last = self.m.screen_hash();
        let mut still = 0;
        for _ in 0..100 {
            self.run_ms(200)?;
            let acted = self.units().iter().any(|u| u.general == g && u.acted);
            let done = match act {
                Act::Rest => acted,
                Act::Attack(..) | Act::Fire(..) => acted && (my_exp(self) != exp0 || all_troops(self) != troops0),
            };
            if done {
                break;
            }
            let h = self.m.screen_hash();
            if h == last {
                still += 1;
                if still >= 3 {
                    self.dismiss()?;
                    still = 0;
                }
            } else {
                still = 0;
                last = h;
            }
        }
        // Let damage / level-up / retreat messages finish before the next command.
        self.settle(2000)?;
        for _ in 0..6 {
            if !(self.unit_selected() || self.dialog_open()) {
                break;
            }
            self.dismiss()?;
            self.settle(1500)?;
        }
        if self.unit_selected() {
            return Err("action did not complete (selection still open)".into());
        }
        let items_after: Vec<Vec<u8>> = self.ours().iter().map(|u| u.items.clone()).collect();
        // Picking up an item (tents) adds one; losing any is never intended, except
        // the fire book that was just used (if the game consumes it).
        // (an item just used — fire book, or a 豆 fed to an ally — is gone)
        let used_book = |a: &Vec<u8>, b: &Vec<u8>| matches!(act, Act::Fire(..)) && a.len() + 1 == b.len();
        if items_after.len() != items_before.len() || items_after.iter().zip(&items_before).any(|(a, b)| a.len() < b.len() && !used_book(a, b)) {
            let _ = self.shot(&format!("dbg_items_changed_{}_{}_{}", g, dest.0, dest.1));
            return Err("items changed — wrong menu entry; rejected".into());
        }
        let after = self.units().into_iter().find(|u| u.general == g);
        match after {
            Some(a) if a.acted && (a.x, a.y) == dest => Ok(()),
            Some(a) => {
                if std::env::var("EIKETSU_BOT_DEBUG").is_ok() {
                    let _ = self.shot("dbg_not_accepted");
                }
                Err(format!("not accepted (acted={} at {},{})", a.acted, a.x, a.y))
            }
            None => Err("unit left the field".into()),
        }
    }

    /// Read every enemy's movement range (once per turn).
    fn scan_enemies(&mut self) -> Result<(), String> {
        let prev_trial = self.m.trial;
        self.m.set_trial(true);
        let enemies = self.enemies();
        let mut out = Vec::new();
        for (row, e) in enemies.iter().enumerate() {
            let at = (e.x, e.y);
            match self.enemy_reach(row as i32, at) {
                Ok(mut r) => {
                    // The shown range is cut short by our units standing in the
                    // way, and they will have moved by the enemy phase: count
                    // every tile within his longest reach as reachable.
                    let m = r.iter().map(|&q| dist(q, at)).max().unwrap_or(0);
                    for dx in -m..=m {
                        for dy in -(m - dx.abs())..=(m - dx.abs()) {
                            let (x, y) = (e.x as i32 + dx, e.y as i32 + dy);
                            if x >= 0 && y >= 0 {
                                r.insert((x as u8, y as u8));
                            }
                        }
                    }
                    out.push((at, r))
                }
                Err(err) => {
                    self.note(format!("T{} enemy {} range unreadable ({err}); assuming 6 tiles", self.turn, e.name));
                    let mut r = HashSet::new();
                    for dx in -6i32..=6 {
                        for dy in -6i32..=6 {
                            if dx.abs() + dy.abs() <= 6 && e.x as i32 + dx >= 0 && e.y as i32 + dy >= 0 {
                                r.insert(((e.x as i32 + dx) as u8, (e.y as i32 + dy) as u8));
                            }
                        }
                    }
                    out.push((at, r));
                }
            }
        }
        self.enemy_ranges = out;
        self.m.set_trial(prev_trial);
        Ok(())
    }

    /// Enemies that can stand next to `t` on their turn.
    fn threats_at(&self, t: (u8, u8)) -> usize {
        self.enemy_ranges.iter().filter(|(at, r)| {
            dist(*at, t) <= 1 || r.iter().any(|&q| dist(q, t) == 1)
        }).count()
    }

    fn candidates(&self, u: &Unit, reach: &HashSet<(u8, u8)>) -> Vec<((u8, u8), Act)> {
        let all = self.units();
        let occupied = |t: (u8, u8)| all.iter().any(|o| (o.x, o.y) == t && o.general != u.general && o.troops > 0);
        // Leave the 鹿砦 open while a farmed general still has to flee onto it, but
        // only once 李肅 is dead: he must fall on flat ground, not heal up there.
        let li_alive = all.iter().any(|o| o.name == "李肅" && o.troops > 0);
        let zhao_off = !li_alive && all.iter().any(|o| FARM.contains(&o.name.as_str()) && o.troops > 0 && (o.x, o.y) != LUJAI);
        let free = |t: (u8, u8)| reach.contains(&t) && (t == (u.x, u.y) || !occupied(t)) && !(zhao_off && t == LUJAI);
        let here = (u.x, u.y);
        let mut c = Vec::new();
        let low = u.troops < self.p.heal_below;
        let last = self.turn >= self.p.final_turn;
        let gate_banned = last && (u.general != 1 || self.finale_kill);
        // Rest spots: healing tiles, tents, staying put, and (when healthy) advancing.
        let enemies = self.enemies();
        let near = |t: (u8, u8)| enemies.iter().map(|e| dist(t, (e.x, e.y))).min().unwrap_or(0);
        let heal_d = |t: (u8, u8)| self.p.heal.iter().map(|&h| dist(t, h)).min().unwrap_or(0);
        let mut rests: Vec<(i32, (u8, u8))> = Vec::new();
        for &t in reach {
            if !free(t) || (gate_banned && t == HUAXIONG_GATE) {
                continue;
            }
            // Weak units head for a healing tile first (resting anywhere else
            // restores nothing), keeping away from enemies second.
            let key = if low || self.caution >= 2 { heal_d(t) * 10 - near(t) } else if self.p.heal.contains(&t) || self.tents_left.contains(&t) { -100 } else { near(t) };
            rests.push((key, t));
        }
        rests.sort();
        for (_, t) in rests.into_iter().take(if low { 4 } else { 5 }) {
            c.push((t, Act::Rest));
        }
        // While 李肅 lives the 鹿砦 is always a candidate (we hold it against him).
        if li_alive && free(LUJAI) && !c.iter().any(|(t, _)| *t == LUJAI) {
            c.push((LUJAI, Act::Rest));
        }
        if !c.iter().any(|(t, _)| *t == here) && !(gate_banned && here == HUAXIONG_GATE) {
            c.push((here, Act::Rest));
        }
        if low || self.caution >= 2 {
            return c;
        }
        for e in &enemies {
            let is_hua = e.name == "華雄";
            if is_hua && u.general == 1 && !last {
                continue; // 關羽 vs 華雄 = duel = end of battle
            }
            if !is_hua && u.general == 1 && last && !self.finale_kill {
                continue; // last turn: 關羽 only goes for the duel
            }
            for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
                let n = ((e.x as i32 + dx) as u8, (e.y as i32 + dy) as u8);
                if is_hua && n != HUAXIONG_GATE {
                    continue;
                }
                if gate_banned && n == HUAXIONG_GATE {
                    continue;
                }
                if free(n) {
                    c.push((n, Act::Attack(e.x, e.y)));
                }
            }
        }
        c
    }

    fn score(&self, before: &[Unit], acting: u16, dest: (u8, u8), act: Act) -> f32 {
        let after = self.units();
        let mut s = 0.0;
        for b in before.iter().filter(|b| b.general < 3) {
            if let Some(a) = after.iter().find(|a| a.general == b.general) {
                s += 5.0 * (total_exp(a) - total_exp(b)) as f32 * self.p.exp_weight.get(b.general as usize).copied().unwrap_or(1.0);
            }
        }
        for b in before.iter().filter(|b| b.side == "敵軍") {
            let a = after.iter().find(|a| a.general == b.general && a.x == b.x && a.y == b.y);
            // Fell to this move (already-dead units do not count again).
            let dead = b.troops > 0 && b.x != 0xff && a.map_or(true, |a| a.troops == 0 || a.x == 0xff);
            // Killing early only wastes EXP if the enemy could have taken more hits.
            if dead && b.name != "華雄" && self.turn < self.p.kill_turn && b.troops > self.p.one_hit {
                s -= 500.0;
            }
            let farm = FARM.contains(&b.name.as_str());
            // 李肅 is not farmed: he dies on flat ground before he can take the 鹿砦.
            if dead && b.name == "李肅" {
                s += 100.0;
            }
            let until = if b.name == "趙岑" { self.p.zhao_kill_turn } else { self.p.kill_turn };
            if dead && farm && self.turn < until {
                s -= 5000.0;
            }
            // 華雄 falls only to 關羽's final-turn duel, or (liu_finale) to 劉備's
            // last hit on the final turn (hua_early: any turn).
            if dead && b.name == "華雄" {
                let in_time = self.p.hua_early || self.turn >= self.p.final_turn;
                s += if self.p.liu_finale && acting == 0 && in_time { 5000.0 } else { -10000.0 };
            }
            if let Some(a) = a {
                let dmg = (b.troops as f32 - a.troops as f32).max(0.0);
                if b.name == "華雄" && self.p.hua_weight != 0.0 {
                    s += self.p.hua_weight * dmg; // wear him down for 劉備's last hit
                } else if b.name == "李肅" {
                    s += 0.1 * dmg;
                } else if b.name == "趙岑" && (b.x, b.y) != LUJAI
                    && self.turn >= self.p.zhao_push_turn && self.turn < self.p.zhao_kill_turn {
                    s += self.p.zhao_weight * dmg; // beat him low so he flees to the 鹿砦
                } else {
                    s -= self.p.damage_weight * dmg;
                }
            }
        }
        let me_before = before.iter().find(|b| b.general == acting).unwrap();
        // Hold the 鹿砦 while 李肅 lives, so he cannot heal up on it.
        if dest == LUJAI && before.iter().any(|b| b.name == "李肅" && b.troops > 0) {
            s += 60.0;
        }
        // 劉備 (last hit) and 關羽 (duel fallback) must reach the gate on the final turn.
        if acting <= 1 && self.turn + 3 >= self.p.final_turn {
            s -= 15.0 * dist(dest, HUAXIONG_GATE) as f32;
        }
        let enemies: Vec<(u8, u8)> = before.iter().filter(|b| b.side == "敵軍" && b.troops > 0).map(|b| (b.x, b.y)).collect();
        // Enemies that can reach a tile next to `dest` on their turn (move ~5) hit for
        // up to ~250 each; 華雄 (3,9) only hits the gate tile.
        // Enemies whose movement range (read from the game) touches `dest`.
        let threats = self.threats_at(dest) as f32;
        let expected = threats.min(self.p.max_hits) * self.p.hit_dmg;
        let left = me_before.troops as f32 - expected;
        // Damage costs healing turns (~220/turn on a village), each worth about one
        // attack's EXP; caution multiplies that.
        let risk_w = self.p.risk_weight * (1 + 3 * self.caution) as f32;
        if left <= 40.0 {
            s -= 1000.0; // likely lost next enemy phase
        } else {
            s -= expected / 220.0 * risk_w;
        }
        // Luring: a spot exactly one healthy enemy can reach brings it to us alone.
        if threats == 1.0 && me_before.troops >= 350 && self.caution == 0 {
            s += self.p.lure_bonus;
        }
        if self.caution >= 2 {
            let near = enemies.iter().map(|&e| dist(dest, e)).min().unwrap_or(0);
            s += 10.0 * near as f32;
        }
        let _ = &enemies;
        if act == Act::Rest {
            let near = |t: (u8, u8)| enemies.iter().map(|&e| dist(t, e)).min().unwrap_or(0);
            if me_before.troops >= self.p.heal_below {
                s += 2.0 * (near((me_before.x, me_before.y)) - near(dest)) as f32;
            }
            if self.tents_left.contains(&dest) {
                s += 40.0;
            }
            if me_before.troops < self.p.heal_below {
                let heal_d = |t: (u8, u8)| self.p.heal.iter().map(|&h| dist(t, h)).min().unwrap_or(0);
                s += 30.0 - 5.0 * heal_d(dest) as f32;
            }
        }
        s
    }

    /// Choose and play one unit's action. Returns a description.
    fn play_unit(&mut self, g: u16) -> Result<String, String> {
        let Some(u) = self.units().into_iter().find(|u| u.general == g && u.side == "我軍") else {
            return Ok("not on field".into());
        };
        if u.acted {
            return Ok("already acted".into());
        }
        let before = self.units();
        let reach = self.select(g)?;
        let snap = self.m.snapshot();
        self.m.set_trial(true);
        let mut best: Option<(f32, (u8, u8), Act)> = None;
        let cands = self.candidates(&u, &reach);
        let n_attacks = cands.iter().filter(|(_, a)| matches!(a, Act::Attack(..))).count();
        for (dest, act) in cands {
            let s = match act {
                // Rest moves are not simulated: the score is the heuristic part only.
                Act::Rest => self.score(&before, g, dest, act),
                Act::Attack(..) | Act::Fire(..) => {
                    let r = self.commit(g, dest, act);
                    let s = match &r {
                        Ok(()) => {
                            let sc = self.score(&before, g, dest, act);
                            if std::env::var("EIKETSU_BOT_DEBUG").is_ok() {
                                let me = self.units().into_iter().find(|x| x.general == g).map(|x| (x.level, x.exp, x.troops));
                                let Act::Attack(tx, ty) = act else { unreachable!() };
                                let tgt = self.units().into_iter().find(|x| x.x == tx && x.y == ty).map(|x| (x.name.clone(), x.troops));
                                eprintln!("[sim] {} {:?} {:?}: score {sc:.1} me(lv,exp,troops)={me:?} target={tgt:?}", u.name, dest, act);
                            }
                            sc
                        }
                        Err(e) => {
                            if std::env::var("EIKETSU_BOT_DEBUG").is_ok() {
                                eprintln!("[cand] {} {:?} {:?}: {e}", u.name, dest, act);
                            }
                            f32::NEG_INFINITY
                        }
                    };
                    self.m.restore(&snap);
                    s
                }
            };
            if std::env::var("EIKETSU_BOT_DEBUG").is_ok() {
                eprintln!("[score] {} {:?} {:?} = {s:.1} (threats {})", u.name, dest, act, self.threats_at(dest));
            }
            if s > f32::NEG_INFINITY && best.as_ref().map_or(true, |b| s > b.0) {
                best = Some((s, dest, act));
            }
        }
        self.m.set_trial(false);
        let Some((s, dest, act)) = best else {
            return Err(format!("{} has no playable action", u.name));
        };
        let commit_res = if act == Act::Rest && (u.troops < self.p.eat_below || self.turn >= self.p.final_turn) {
            self.commit_rest_or_bean(g, dest)
        } else {
            self.commit(g, dest, act)
        };
        if let Err(e) = commit_res {
            // Fall back to staying put so the turn can go on.
            self.m.restore(&snap);
            if let Err(e2) = self.commit(g, (u.x, u.y), Act::Rest) {
                self.ensure_idle()?;
                self.select(g)?;
                self.commit(g, (u.x, u.y), Act::Rest).map_err(|e3| format!("{e}; fallback: {e2}; retry: {e3}"))?;
            }
            return Ok(format!("{} stayed (chosen {:?} {:?} failed: {e})", u.name, dest, act));
        }
        if act == Act::Rest {
            self.tents_left.retain(|t| *t != dest);
        }
        Ok(format!("{} → {:?} {:?} (score {s:.1}, {n_attacks} attack options)", u.name, dest, act))
    }

    fn all_acted(&self) -> bool {
        let ours = self.ours();
        !ours.is_empty() && ours.iter().all(|u| u.acted)
    }

    fn end_turn(&mut self) -> Result<(), String> {
        // Once every unit has acted the game itself asks 「結束所有部隊的命令嗎？」
        // (after its own delay); answer 是.
        self.settle(2000)?;
        let auto = self.all_acted() && self.wait_until(|b: &Bot| b.dialog_open(), 8000)?;
        if auto {
            return self.answer_end_dialog();
        }
        self.ensure_idle()?;
        let mut opened = false;
        for _ in 0..3 {
            if self.dialog_open() {
                return self.answer_end_dialog();
            }
            self.click(45, 55, 400)?;
            if self.wait_until(|b: &Bot| b.func_menu_open() || b.dialog_open(), 1500)? {
                if self.dialog_open() {
                    return self.answer_end_dialog();
                }
                opened = true;
                break;
            }
        }
        if !opened {
            let _ = self.shot("dbg_no_func_menu");
            let st = battle::describe(&self.m.mem);
            return Err(format!("功能 menu did not open; dialog={} selected={} menu={}
{st}", self.dialog_open(), self.unit_selected(), self.find_menu().is_some()));
        }
        self.click(85, 112, 400)?;
        if !self.wait_until(|b: &Bot| b.dialog_open(), 1500)? {
            self.click(85, 112, 400)?;
        }
        if !self.wait_until(|b: &Bot| b.dialog_open(), 1500)? {
            let _ = self.shot("dbg_no_end_dialog");
            return Err("end-turn dialog did not appear".into());
        }
        self.answer_end_dialog()
    }

    /// 「結束所有部隊的命令嗎？」 → 是, then the enemy phase.
    fn answer_end_dialog(&mut self) -> Result<(), String> {
        for _ in 0..3 {
            self.click(222, 232, 400)?;
            if self.wait_until(|b: &Bot| !b.dialog_open(), 1500)? {
                break;
            }
        }
        self.enemy_phase()
    }

    fn enemy_phase(&mut self) -> Result<(), String> {
        // Enemy phase: wait until every unit of ours can act again. Messages
        // (unit lost, level up …) need a click; the map ignores clicks meanwhile.
        for i in 0..400 {
            self.run_ms(300)?;
            let ours = self.ours();
            // One of ours knocked out (劉備 out ends the game with 「再玩一次嗎？」):
            // stop here, the caller sees the loss and replays the turn.
            if ours.len() < 3 || ours.iter().any(|u| u.troops == 0) {
                return Ok(());
            }
            if ours.iter().all(|u| !u.acted) {
                self.settle(4000)?;
                return Ok(());
            }
            // Story dialogs have a blinking cursor, so the picture never looks
            // still: just knock on them regularly.
            if i % 5 == 4 {
                self.dismiss()?;
            }
        }
        let _ = self.shot("stuck_enemy_phase");
        Err("enemy phase did not finish (see stuck_enemy_phase.png)".into())
    }

    fn summary(&self) -> String {
        self.ours().iter().map(|u| format!("{} LV{} EXP{} 兵{}", u.name, u.level, u.exp, u.troops)).collect::<Vec<_>>().join(", ")
    }

    /// Play the whole battle. Returns the total EXP gained.
    /// Save state + bot bookkeeping after a turn.
    fn save_turn(&mut self) -> Result<(), String> {
        let p = self.out.join(format!("state_T{:02}.bin", self.turn));
        self.m.save_state(&p)?;
        let meta = format!("turn={}\ntents={}\nstart_exp={}\n", self.turn, self.tents_left.iter().map(|t| format!("{},{}", t.0, t.1)).collect::<Vec<_>>().join(";"), self.start_exp);
        std::fs::write(p.with_extension("txt"), meta).map_err(|e| e.to_string())
    }

    fn resume_from(&mut self, p: &PathBuf) -> Result<(), String> {
        self.m.load_state(p)?;
        if self.p.clear_journal {
            self.m.clear_journal();
        }
        if let Ok(meta) = std::fs::read_to_string(p.with_extension("txt")) {
            for line in meta.lines() {
                if let Some(v) = line.strip_prefix("turn=") { self.turn = v.parse::<u32>().map_err(|e| e.to_string())? + 1 }
                if let Some(v) = line.strip_prefix("start_exp=") { self.start_exp = v.parse().map_err(|e: std::num::ParseIntError| e.to_string())? }
                if let Some(v) = line.strip_prefix("tents=") {
                    self.tents_left = v.split(';').filter(|t| !t.is_empty()).filter_map(|t| {
                        let (a, b) = t.split_once(',')?;
                        Some((a.parse().ok()?, b.parse().ok()?))
                    }).collect();
                }
            }
        } else {
            self.turn = 1;
            self.start_exp = self.ours().iter().map(total_exp).sum();
        }
        self.note(format!("resumed from {} at turn {}: {}", p.display(), self.turn, self.summary()));
        self.note(battle::describe(&self.m.mem));
        if let Some((x, y)) = self.p.intro_click.filter(|_| self.turn == 1) {
            // A state saved on the battle's opening 勝利條件 box: press 確認.
            // (Only before the first turn: later the clicks would land on units.)
            self.settle(1500)?;
            self.click(x, y, 800)?;
            self.settle(2000)?;
            // Then the opening dialogue: knock on it until our units can be
            // selected (the 我軍 list answers).
            for _ in 0..30 {
                self.click(240, 140, 1200)?;
                self.click(545, 88, 300)?;
                self.click(470, 122, 600)?;
                if self.unit_selected() {
                    self.ensure_idle()?;
                    break;
                }
                // Still not in control: the 勝利條件 box comes back after the talk.
                self.click(x, y, 1200)?;
            }
        }
        self.set_battle_options()?;
        if let Ok(v) = std::env::var("EIKETSU_BOT_BOSS_TROOPS") {
            // Probe only: set the boss's troops (e.g. to measure the kill EXP).
            let n: u16 = v.parse().map_err(|_| "EIKETSU_BOT_BOSS_TROOPS=n")?;
            let gt = battle::general_table(&self.m.mem).ok_or("no general table")?;
            let b = self.enemies().into_iter().find(|e| e.name == self.p.boss).ok_or("no boss")?;
            let at = (gt + b.general as u32 * battle::GEN_SIZE + 0x19) as usize;
            self.m.mem.ram[at..at + 2].copy_from_slice(&n.to_le_bytes());
            self.note(format!("probe: {} troops set to {n}", self.p.boss));
        }
        if let Ok(v) = std::env::var("EIKETSU_BOT_POKE") {
            // Probe only: write a byte at general table + offset ("offset,value", hex offset).
            let (o, b) = v.split_once(',').ok_or("EIKETSU_BOT_POKE=offset,value")?;
            let gt = battle::general_table(&self.m.mem).ok_or("no general table")?;
            let o = u32::from_str_radix(o.trim_start_matches("0x"), 16).map_err(|_| "poke offset")?;
            self.m.mem.ram[(gt + o) as usize] = b.parse().map_err(|_| "poke value")?;
            self.note(format!("poked gt+{o:#x} = {b}"));
        }
        if let Ok(g) = std::env::var("EIKETSU_BOT_STRAT") {
            // Probe: open unit g's 策略 list in place and screenshot it (no cast).
            let g: u16 = g.parse().map_err(|_| "EIKETSU_BOT_STRAT=general index")?;
            let u = self.units().into_iter().find(|u| u.general == g).ok_or("no such unit")?;
            self.select(g)?;
            self.tap((u.x, u.y), 300)?;
            self.wait_until(|b: &Bot| !b.unit_selected(), 1500)?;
            self.run_ms(300)?;
            let (px, py0) = battle::tile_screen(&self.m.mem, u.x as i32, u.y as i32).ok_or("not in view")?;
            let mx = if px >= 224 { px - 57 } else { px + 57 };
            let top = (py0 - 16).clamp(72, 424 - 112);
            // EIKETSU_BOT_STRAT_ROW: 43 = 策略, 67 = 道具.
            let row: i32 = std::env::var("EIKETSU_BOT_STRAT_ROW").ok().and_then(|v| v.parse().ok()).unwrap_or(43);
            self.note(format!("probe menu: tile {:?} px ({px},{py0}) mx {mx} top {top}", (u.x, u.y)));
            self.click(mx, top + row, 600)?;
            self.settle(1500)?;
            self.shot(&format!("strat_{g}_{row}"))?;
            // EIKETSU_BOT_STRAT_CLICKS="x,y;x,y": further clicks, a screenshot after each.
            if let Ok(pts) = std::env::var("EIKETSU_BOT_STRAT_CLICKS") {
                for (i, pt) in pts.split(';').enumerate() {
                    let (x, y) = pt.split_once(',').ok_or("bad point")?;
                    self.click(x.parse().map_err(|_| "x")?, y.parse().map_err(|_| "y")?, 600)?;
                    self.settle(2500)?;
                    self.shot(&format!("strat_{g}_{row}_{i}"))?;
                    self.note(format!("probe click {pt}: {}", self.summary()));
                }
            }
        }
        if std::env::var("EIKETSU_BOT_GRID").is_ok() {
            self.shot("resume")?;
        }
        if let Ok(pts) = std::env::var("EIKETSU_BOT_MINIMAP") {
            for pt in pts.split(';') {
                let (x, y) = pt.split_once(',').ok_or("bad point")?;
                let (x, y): (i32, i32) = (x.parse().map_err(|_| "x")?, y.parse().map_err(|_| "y")?);
                self.click(x, y, 300)?;
                self.settle(1500)?;
                self.note(format!("minimap click ({x},{y}) -> view {:?}", battle::view(&self.m.mem)));
                self.shot(&format!("mm_{x}_{y}"))?;
            }
        }
        if std::env::var("EIKETSU_BOT_SCAN").is_ok() {
            self.scan_enemies()?;
            for (at, r) in self.enemy_ranges.clone() {
                let mut v: Vec<_> = r.into_iter().collect();
                v.sort();
                self.note(format!("enemy at {at:?} reach {v:?}"));
            }
        }
        if let Ok(f) = std::env::var("EIKETSU_BOT_RAM") {
            let gt = battle::general_table(&self.m.mem).unwrap_or(0);
            std::fs::write(&f, &self.m.mem.ram).map_err(|e| e.to_string())?;
            self.note(format!("ram dumped to {f}; general table at {gt:#x}"));
        }
        Ok(())
    }

    /// Commit a Rest move to `t`, or if `eat`/`feed` is enabled and `g` holds a 豆 (0x1e),
    /// try using the 豆 on itself (or an adjacent injured ally) at `t` for +8 EXP + healing;
    /// falls back cleanly to `Act::Rest` if the bean cannot be used.
    fn commit_rest_or_bean(&mut self, g: u16, t: (u8, u8)) -> Result<(), String> {
        if self.p.eat || self.p.feed {
            if let Some(u) = self.units().into_iter().find(|u| u.general == g) {
                if let Some(slot) = u.items.iter().position(|&i| i == 0x1e) {
                    let mut targets = Vec::new();
                    if self.p.eat && u.troops < self.p.eat_below {
                        targets.push(t);
                    }
                    if self.p.feed {
                        let mut allies: Vec<Unit> = self.ours().into_iter()
                            .filter(|a| a.general != g && a.troops > 0 && a.troops < self.p.feed_below && dist(t, (a.x, a.y)) == 1)
                            .collect();
                        allies.sort_by_key(|a| a.troops);
                        for a in allies {
                            targets.push((a.x, a.y));
                        }
                    }
                    for tgt in targets {
                        let snap = self.m.snapshot();
                        let exp_before = self.ours().iter().map(total_exp).sum::<i32>();
                        let troops_before: u32 = self.ours().iter().map(|u| u.troops as u32).sum();
                        if self.commit(g, t, Act::Fire(tgt.0, tgt.1, slot as u8)).is_ok() {
                            let exp_after = self.ours().iter().map(total_exp).sum::<i32>();
                            let troops_after: u32 = self.ours().iter().map(|u| u.troops as u32).sum();
                            if troops_after > troops_before || exp_after > exp_before {
                                return Ok(());
                            }
                        }
                        self.m.restore(&snap);
                    }
                }
            }
        }
        self.commit(g, t, Act::Rest)
    }

    /// Try 劉備 attacking 華雄 from the gate on a snapshot; true if 華雄 falls.
    /// The game is restored either way.
    fn trial_liubei_kill(&mut self) -> Result<bool, String> {
        if !self.enemies().iter().any(|e| e.name == "華雄") {
            return Ok(false);
        }
        let snap = self.m.snapshot();
        self.m.set_trial(true);
        let r = match self.select(0) {
            Ok(reach) if reach.contains(&HUAXIONG_GATE) => self.commit(0, HUAXIONG_GATE, Act::Attack(3, 9)),
            Ok(_) => Err("gate out of reach".into()),
            Err(e) => Err(e),
        };
        self.run_ms(3000)?;
        let killed = !self.enemies().iter().any(|e| e.name == "華雄");
        self.note(format!("T{} trial 劉備 → 華雄: {r:?}, killed={killed}", self.turn));
        self.m.restore(&snap);
        self.m.set_trial(false);
        Ok(killed && !matches!(&r, Err(e) if e == "gate out of reach"))
    }

    /// 胡軫 sits on the 鹿砦 (before kill_turn): the farm. Plans are tried with as
    /// many attackers as possible first (each from its own tile next to the 鹿砦;
    /// units below farm_min troops go to a healing tile instead); each plan is
    /// played through the enemy phase on a snapshot. The first attacker count that
    /// gives a safe outcome with 胡軫 still on the 鹿砦 wins (best EXP among its
    /// plans). At most farm_trials plans per turn. None when no farm turn is due.
    fn farm_turn(&mut self) -> Result<Option<String>, String> {
        let en = self.enemies();
        let Some(hu) = en.iter().find(|e| e.name == "胡軫") else { return Ok(None) };
        let hu_at = (hu.x, hu.y);
        // From kill_turn on he may fall: everyone able hits him, in every order.
        let kill = self.turn >= self.p.kill_turn;
        if kill && self.turn >= self.p.final_turn {
            return Ok(None);
        }
        if self.p.lujai_ours {
            // 鹿砦 kept as our healing station: fight him on open ground once
            // the others are gone and he is out of the gate.
            if en.iter().any(|e| e.name != "華雄" && e.name != "胡軫") || hu.x <= 6 {
                return Ok(None);
            }
        } else if hu_at != LUJAI {
            return Ok(None);
        }
        self.farm_at = hu_at;
        let lujai_free = hu_at != LUJAI;
        self.farm_heal = if lujai_free { vec![LUJAI, (13, 12), (19, 9)] } else { FARM_HEAL.to_vec() };
        let mut ours = self.ours();
        ours.retain(|u| !u.acted && u.troops > 0);
        ours.sort_by_key(|u| std::cmp::Reverse(u.troops));
        let min = if kill { 150 } else { self.p.farm_min };
        let fit: Vec<u16> = ours.iter().filter(|u| u.troops >= min).map(|u| u.general).collect();
        let weak: Vec<u16> = ours.iter().filter(|u| u.troops < min).map(|u| u.general).collect();
        // Tiles next to him (never next to 華雄 at the gate).
        let sides: Vec<(u8, u8)> = neighbours(hu_at).into_iter().filter(|&t| dist(t, (3, 9)) > 1).collect();
        // Which of those tiles each healthy unit can stand on this turn.
        let start = self.m.snapshot();
        self.m.set_trial(true);
        let mut can: std::collections::HashMap<u16, Vec<(u8, u8)>> = Default::default();
        for &g in &fit {
            let reach = self.select(g)?;
            let taken: HashSet<(u8, u8)> = self.units().iter()
                .filter(|o| o.troops > 0 && o.general != g).map(|o| (o.x, o.y)).collect();
            can.insert(g, sides.iter().copied().filter(|t| reach.contains(t) && !taken.contains(t)).collect());
            self.ensure_idle()?;
        }
        // Too weak to take a hit: nobody attacks, the healthy wait next to him.
        let hold = !kill && hu.troops < self.p.farm_hu_min;
        let exp0: i32 = self.ours().iter().map(total_exp).sum();
        let mut tried = 0;
        let mut best: Option<(f32, crate::machine::Snapshot, String)> = None;
        let kmax = if hold { 0 } else { fit.len() };
        for k in (0..=kmax).rev() {
            // Attackers: k of the healthy units (healthiest first); the other
            // healthy ones wait next to the 鹿砦, the weak heal.
            let mut plans: Vec<(Vec<u16>, Vec<(u8, u8)>)> = Vec::new();
            let mut combos: Vec<Vec<u16>> = Vec::new();
            choose(&fit, k, &mut Vec::new(), &mut combos);
            if kill {
                // Who strikes last matters (the kill EXP): every order.
                let mut ordered = Vec::new();
                for c in &combos {
                    let mut o: Vec<Vec<u16>> = Vec::new();
                    order_all(c, &mut Vec::new(), &mut o);
                    ordered.extend(o);
                }
                combos = ordered;
            }
            // A weakened 胡軫 is hit by the lighter hitters first (attack power
            // as the game shows it: 劉備 265, 關羽 378, 張飛 373).
            if hu.troops < 300 {
                const ATK: [u32; 3] = [265, 378, 373];
                combos.sort_by_key(|c| c.iter().map(|&g| ATK.get(g as usize).copied().unwrap_or(400)).sum::<u32>());
            }
            // Round-robin over attacker sets, so every set gets tried before a
            // set's other tile orders (the tile hardly changes the damage).
            let mut per_set: Vec<Vec<(Vec<u16>, Vec<(u8, u8)>)>> = Vec::new();
            for att in combos {
                let mut orders: Vec<Vec<(u8, u8)>> = Vec::new();
                permute(&sides, k, &mut Vec::new(), &mut orders);
                per_set.push(orders.into_iter()
                    .filter(|tiles| att.iter().zip(tiles).all(|(g, t)| can[g].contains(t)))
                    .take(2)
                    .map(|tiles| (att.clone(), tiles)).collect());
            }
            for i in 0..per_set.iter().map(Vec::len).max().unwrap_or(0) {
                for set in &per_set {
                    if let Some(pl) = set.get(i) {
                        plans.push(pl.clone());
                    }
                }
            }
            let mut killers: Vec<Vec<u16>> = Vec::new();
            for (attackers, tiles) in plans.into_iter().take(if kill { 8 } else { 4 }) {
                if tried >= self.p.farm_trials {
                    break;
                }
                if !kill && killers.contains(&attackers) {
                    continue; // this set already killed him from another tile
                }
                tried += 1;
                self.m.restore(&start);
                let waiters: Vec<u16> = fit.iter().copied().filter(|g| !attackers.contains(g)).collect();
                let attackers = &attackers[..];
                let r = self.farm_trial(attackers, &tiles, &waiters, &weak);
                let (score, desc) = match r {
                    Ok(()) => {
                        let ours = self.ours();
                        let safe = ours.len() == 3 && ours.iter().all(|u| u.troops > self.p.floor);
                        let hu2 = self.enemies().into_iter().find(|e| e.name == "胡軫");
                        let kept = kill || hu2.as_ref().map_or(false, |h| lujai_free || (h.x, h.y) == LUJAI);
                        if hu2.is_none() {
                            killers.push(attackers.to_vec());
                        }
                        let gained = ours.iter().map(total_exp).sum::<i32>() - exp0;
                        // Back at full strength he leaves the 鹿砦 to hunt: keep him hurt.
                        let full = !kill && !lujai_free && hu2.as_ref().map_or(false, |h| h.troops >= self.p.farm_hu_max);
                        let sc = if safe && kept { 0.0 } else { -1e6 } + if full { -500.0 } else { 0.0 } + 10.0 * gained as f32
                            + 0.05 * ours.iter().map(|u| u.troops as f32).sum::<f32>();
                        let plan: Vec<String> = attackers.iter().zip(&tiles).map(|(g, t)| format!("{g}@{t:?}")).collect();
                        (sc, format!("{} attack [{}], EXP +{gained}, 胡軫 {:?}, {}", k, plan.join(" "),
                            hu2.map(|h| ((h.x, h.y), h.troops)), self.summary()))
                    }
                    Err(e) => {
                        if e == "killed early" {
                            killers.push(attackers.to_vec());
                        }
                        (f32::NEG_INFINITY, format!("{k} attack {tiles:?}: {e}"))
                    }
                };
                self.note(format!("T{} farm trial: {desc} (score {score:.0})", self.turn));
                if score > f32::NEG_INFINITY && best.as_ref().map_or(true, |x| score > x.0) {
                    best = Some((score, self.m.snapshot(), desc));
                }
            }
            if best.as_ref().map_or(false, |b| b.0 > -1e5) || tried >= self.p.farm_trials {
                break;
            }
        }
        match best {
            Some((_, snap, desc)) => {
                self.m.finish_turn(&start, &snap, self.p.live);
                Ok(Some(desc))
            }
            None => {
                self.m.restore(&start);
                self.m.set_trial(false);
                Ok(None)
            }
        }
    }

    /// One farm plan: attackers hit 胡軫 from their tiles, waiters (healthy but not
    /// attacking) rest next to the 鹿砦 (or where they are), healers go to the
    /// nearest free healing tile they can reach (else as far from him as they can).
    fn farm_trial(&mut self, attackers: &[u16], tiles: &[(u8, u8)], waiters: &[u16], healers: &[u16]) -> Result<(), String> {
        let kill = self.turn >= self.p.kill_turn;
        for (&g, &t) in attackers.iter().zip(tiles) {
            if !self.enemies().iter().any(|e| (e.x, e.y) == self.farm_at) {
                if !kill {
                    return Err("killed early".into());
                }
                break;
            }
            self.select(g)?;
            self.commit(g, t, Act::Attack(self.farm_at.0, self.farm_at.1))?;
            if !kill && !self.enemies().iter().any(|e| (e.x, e.y) == self.farm_at) {
                return Err("killed early".into());
            }
        }
        for &g in waiters {
            let Some(u) = self.units().into_iter().find(|u| u.general == g) else { continue };
            let reach = self.select(g)?;
            let taken: HashSet<(u8, u8)> = self.units().iter()
                .filter(|o| o.troops > 0 && o.general != g).map(|o| (o.x, o.y)).collect();
            // Next to the 鹿砦 if possible, else as close to a free side as they get.
            let free_sides: Vec<(u8, u8)> = neighbours(self.farm_at).into_iter().filter(|t| !taken.contains(t)).collect();
            let feed_adj: Vec<(u8, u8)> = if self.p.feed && u.items.contains(&0x1e) {
                self.ours().into_iter()
                    .filter(|a| a.general != g && a.troops > 0 && a.troops < self.p.feed_below)
                    .flat_map(|a| neighbours((a.x, a.y)))
                    .filter(|t| reach.contains(t) && !taken.contains(t) && *t != self.farm_at && dist(*t, (3, 9)) > 1)
                    .collect()
            } else {
                Vec::new()
            };
            let t = if !feed_adj.is_empty() && !(self.p.eat && u.troops < self.p.eat_below) {
                feed_adj.into_iter().min_by_key(|&t| (dist(t, self.farm_at), t)).unwrap_or((u.x, u.y))
            } else if neighbours(self.farm_at).contains(&(u.x, u.y)) {
                (u.x, u.y)
            } else {
                reach.iter().copied()
                    .filter(|t| !taken.contains(t) && *t != self.farm_at && dist(*t, (3, 9)) > 1)
                    .min_by_key(|&t| (free_sides.iter().map(|&q| dist(t, q)).min().unwrap_or(99), t))
                    .unwrap_or((u.x, u.y))
            };
            self.commit_rest_or_bean(g, t)?;
        }
        for &g in healers {
            let Some(u) = self.units().into_iter().find(|u| u.general == g) else { continue };
            let reach = self.select(g)?;
            let taken: HashSet<(u8, u8)> = self.units().iter()
                .filter(|o| o.troops > 0 && o.general != g).map(|o| (o.x, o.y)).collect();
            if self.p.eat && u.troops < self.p.eat_below {
                if let Some(slot) = u.items.iter().position(|&i| i == 0x1e) {
                    let t_bean = reach.iter().copied()
                        .filter(|t| !taken.contains(t) && *t != self.farm_at && dist(*t, (3, 9)) > 1)
                        .min_by_key(|&t| ((dist(t, self.farm_at) - 2).abs(), dist(t, (u.x, u.y)), t))
                        .unwrap_or((u.x, u.y));
                    let snap = self.m.snapshot();
                    let troops_before: u32 = self.ours().iter().map(|x| x.troops as u32).sum();
                    if self.commit(g, t_bean, Act::Fire(t_bean.0, t_bean.1, slot as u8)).is_ok() {
                        let troops_after: u32 = self.ours().iter().map(|x| x.troops as u32).sum();
                        if troops_after > troops_before {
                            continue;
                        }
                    }
                    self.m.restore(&snap);
                }
            }
            // A free healing tile in reach; otherwise head for the nearest one.
            let free_heal: Vec<(u8, u8)> = self.farm_heal.iter().copied().filter(|t| !taken.contains(t)).collect();
            let t = free_heal.iter().copied().find(|t| reach.contains(t)).unwrap_or_else(|| reach.iter().copied()
                .filter(|t| !taken.contains(t) && *t != self.farm_at)
                .min_by_key(|&t| (free_heal.iter().map(|&h| dist(t, h)).min().unwrap_or(0), t))
                .unwrap_or((u.x, u.y)));
            self.commit(g, t, Act::Rest)?;
        }
        self.end_turn()
    }

    /// Bring 關羽 next to the gate (5,8) for the final-turn duel, before anyone
    /// else moves (never onto the gate itself; he does not attack).
    /// Other maps: `finale_g` closes in on the boss instead (never next to him).
    fn guan_to_gate(&mut self) -> Result<(), String> {
        let (g, goal, min_d) = if self.p.boss == "華雄" {
            (1, (5, 8), 0)
        } else {
            let Some(b) = self.enemies().into_iter().find(|e| e.name == self.p.boss) else { return Ok(()) };
            (self.p.finale_g, (b.x, b.y), 2)
        };
        let Some(u) = self.units().into_iter().find(|u| u.general == g && u.side == "我軍") else { return Ok(()) };
        if u.acted || u.troops == 0 {
            return Ok(());
        }
        let reach = self.select(g)?;
        let taken: HashSet<(u8, u8)> = self.units().iter()
            .filter(|o| o.troops > 0 && o.general != g).map(|o| (o.x, o.y)).collect();
        let t = reach.iter().copied()
            .filter(|t| !taken.contains(t) && *t != HUAXIONG_GATE && dist(*t, goal) >= min_d)
            // Out of everyone's reach first (呂布 wipes out a unit in one blow).
            .min_by_key(|&t| (if min_d > 0 { self.threats_at(t) } else { 0 }, dist(t, goal), t))
            .unwrap_or((u.x, u.y));
        self.commit_rest_or_bean(g, t)?;
        self.note(format!("T{} {g} → {t:?} (waiting for the duel)", self.turn));
        Ok(())
    }

    /// Endgame (李肅, 趙岑 and 胡軫 gone, before the final turn): one of 劉備 / 張飛
    /// hits 華雄 from the gate (EXP for his level), the others heal on the freed
    /// 鹿砦 or close in on the gate; 關羽 waits next to the gate for the final-turn
    /// duel and never touches 華雄 before. Each choice of hitter (or none) is
    /// played through the enemy phase on a snapshot; the best safe one is kept.
    fn hua_turn(&mut self) -> Result<Option<String>, String> {
        let en = self.enemies();
        if en.iter().any(|e| e.name != "華雄") || !en.iter().any(|e| e.name == "華雄")
            || self.turn >= self.p.final_turn {
            return Ok(None);
        }
        let ours: Vec<Unit> = self.ours().into_iter().filter(|u| !u.acted && u.troops > 0).collect();
        if ours.is_empty() {
            return Ok(None);
        }
        let exp0: i32 = self.ours().iter().map(total_exp).sum();
        let start = self.m.snapshot();
        self.m.set_trial(true);
        let mut best: Option<(f32, crate::machine::Snapshot, String)> = None;
        let hitters: Vec<Option<u16>> = vec![Some(2), Some(0), None];
        for hitter in hitters {
            if let Some(h) = hitter {
                if !ours.iter().any(|u| u.general == h) {
                    continue;
                }
            }
            self.m.restore(&start);
            let r = self.hua_trial(hitter, &ours);
            let (score, desc) = match r {
                Ok(()) => {
                    let now = self.ours();
                    let safe = now.len() == 3 && now.iter().all(|u| u.troops > self.p.floor);
                    let gained = now.iter().map(total_exp).sum::<i32>() - exp0;
                    let sc = if safe { 0.0 } else { -1e6 } + 10.0 * gained as f32
                        + 0.05 * now.iter().map(|u| u.troops as f32).sum::<f32>();
                    (sc, format!("hitter {hitter:?}: EXP +{gained}, {}", self.summary()))
                }
                Err(e) => (f32::NEG_INFINITY, format!("hitter {hitter:?}: {e}")),
            };
            self.note(format!("T{} 華雄 trial: {desc} (score {score:.0})", self.turn));
            if score > f32::NEG_INFINITY && best.as_ref().map_or(true, |x| score > x.0) {
                best = Some((score, self.m.snapshot(), desc));
            }
        }
        match best {
            Some((_, snap, desc)) => {
                self.m.finish_turn(&start, &snap, self.p.live);
                Ok(Some(desc))
            }
            None => {
                self.m.restore(&start);
                self.m.set_trial(false);
                Ok(None)
            }
        }
    }

    fn hua_trial(&mut self, hitter: Option<u16>, ours: &[Unit]) -> Result<(), String> {
        let mut order: Vec<u16> = ours.iter().map(|u| u.general).collect();
        let at_gate = |g: u16| ours.iter().any(|u| u.general == g && (u.x, u.y) == HUAXIONG_GATE);
        order.sort_by_key(|&g| (!(at_gate(g) && Some(g) != hitter), Some(g) != hitter, g == 1));
        for g in order {
            let Some(u) = self.units().into_iter().find(|u| u.general == g) else { continue };
            let reach = self.select(g)?;
            if Some(g) == hitter {
                self.commit(g, HUAXIONG_GATE, Act::Attack(3, 9))?;
                continue;
            }
            let taken: HashSet<(u8, u8)> = self.units().iter()
                .filter(|o| o.troops > 0 && o.general != g).map(|o| (o.x, o.y)).collect();
            // 關羽 waits at (5,8) for the duel; the weak heal on the 鹿砦 (unless they hold a 豆);
            // the rest close in on the gate (never onto it).
            let has_bean = self.p.eat && u.items.contains(&0x1e);
            let goal = if g == 1 { (5, 8) } else if u.troops < self.p.hua_min && !has_bean { LUJAI } else { (5, 10) };
            let t = reach.iter().copied()
                .filter(|t| !taken.contains(t) && *t != HUAXIONG_GATE)
                .min_by_key(|&t| (dist(t, goal), t))
                .unwrap_or((u.x, u.y));
            self.commit_rest_or_bean(g, t)?;
        }
        self.end_turn()
    }

    /// General turn planner (any map, gplan=1): each healthy unit gets its two
    /// best-looking attacks (higher-level targets first, then weaker ones), weak
    /// units head for the nearest free healing tile. Combinations of those are
    /// played through the enemy phase on snapshots; the best safe one (weighted
    /// EXP, then troops) is kept. The boss must not fall before kill_turn.
    fn gplan_turn(&mut self) -> Result<Option<String>, String> {
        if !self.p.gplan || self.turn >= self.p.final_turn {
            return Ok(None);
        }
        let en = self.enemies();
        if en.is_empty() {
            return Ok(None);
        }
        // pickup=g@x,y: unit g first walks onto that tile (a tent's item) until it holds one more item.
        if let Some((g, tile)) = self.p.pickup {
            if let Some(u) = self.ours().into_iter().find(|u| u.general == g && !u.acted && u.troops > 0) {
                if self.pickup_items.map_or(true, |n| u.items.len() <= n) {
                    self.pickup_items.get_or_insert(u.items.len());
                    let reach = self.select(g)?;
                    let taken: HashSet<(u8, u8)> = self.units().iter()
                        .filter(|o| o.troops > 0 && o.general != g).map(|o| (o.x, o.y)).collect();
                    let t = if reach.contains(&tile) && !taken.contains(&tile) { tile } else {
                        reach.iter().copied().filter(|t| !taken.contains(t))
                            .min_by_key(|&t| (dist(t, tile), self.threats_at(t), t)).unwrap_or((u.x, u.y))
                    };
                    self.commit(g, t, Act::Rest)?;
                    let n = self.ours().into_iter().find(|u| u.general == g).map(|u| u.items.len()).unwrap_or(0);
                    self.note(format!("T{} {g} → {t:?} for the item at {tile:?} (items {n})", self.turn));
                }
            }
        }
        let mut ours = self.ours();
        ours.retain(|u| !u.acted && u.troops > 0);
        ours.sort_by_key(|u| std::cmp::Reverse(u.troops));
        let fit: Vec<u16> = ours.iter().filter(|u| u.troops >= self.p.farm_min).map(|u| u.general).collect();
        let weak: Vec<u16> = ours.iter().filter(|u| u.troops < self.p.farm_min).map(|u| u.general).collect();
        // Attack options per healthy unit, from its real movement range.
        let start = self.m.snapshot();
        self.m.set_trial(true);
        let mut opts: std::collections::HashMap<u16, Vec<((u8, u8), (u8, u8), bool)>> = Default::default();
        for &g in &fit {
            let reach = self.select(g)?;
            let taken: HashSet<(u8, u8)> = self.units().iter()
                .filter(|o| o.troops > 0 && o.general != g).map(|o| (o.x, o.y))
                .chain(self.p.avoid.iter().copied()).collect();
            // The boss is left alone (on 虎牢關 張飛 attacking 呂布 starts the scripted
            // duel, which ends the battle); with boss_hit the others may hit him
            // while he has troops to spare.
            let mut targets: Vec<&Unit> = en.iter().filter(|t| t.name != self.p.boss
                || (self.p.boss_hit && g != self.p.finale_g && t.troops >= self.p.boss_hit_min
                    // he strikes back at whoever stands next to him (~470 troops)
                    && self.units().iter().any(|u| u.general == g && u.troops >= self.p.boss_hit_troops))).collect();
            targets.sort_by_key(|t| (std::cmp::Reverse(t.level), t.troops));
            let mut o = Vec::new();
            for t in targets {
                if let Some(tile) = neighbours((t.x, t.y)).into_iter()
                    .filter(|n| reach.contains(n) && !taken.contains(n))
                    .min_by_key(|&n| (self.threats_at(n), n)) {
                    o.push(((t.x, t.y), tile, false));
                    // Holding a fire book on a dry day: the same strike with fire.
                    if let Some(u) = self.units().into_iter().find(|u| u.general == g) {
                        let dry = battle::general_table(&self.m.mem).map_or(false, |gt| self.m.mem.ram[(gt + WEATHER) as usize] < 3);
                        if dry && u.items.contains(&FIRE_BOOK) {
                            o.push(((t.x, t.y), tile, true));
                        }
                    }
                }
                if o.len() >= 2 {
                    break;
                }
            }
            opts.insert(g, o);
            self.ensure_idle()?;
        }
        // Plans: every unit picks one of its options or none (heal / hold).
        let mut plans: Vec<Vec<(u16, Option<((u8, u8), (u8, u8), bool)>)>> = vec![Vec::new()];
        for &g in &fit {
            let mut next = Vec::new();
            for pl in &plans {
                for choice in opts[&g].iter().map(|&c| Some(c)).chain(std::iter::once(None)) {
                    if let Some((_, tile, _)) = choice {
                        if pl.iter().any(|(_, c): &(u16, Option<((u8, u8), (u8, u8), bool)>)| c.map_or(false, |c| c.1 == tile)) {
                            continue;
                        }
                    }
                    let mut p2 = pl.clone();
                    p2.push((g, choice));
                    next.push(p2);
                }
            }
            plans = next;
        }
        // Most attacks first.
        plans.sort_by_key(|pl| std::cmp::Reverse(pl.iter().filter(|(_, c)| c.is_some()).count()));
        let w = self.p.exp_weight.clone();
        let wexp = |b: &Bot| -> f32 { b.ours().iter().map(|u| total_exp(u) as f32 * w.get(u.general as usize).copied().unwrap_or(1.0)).sum() };
        let exp0 = wexp(self);
        let minions = |b: &Bot| b.enemies().iter().filter(|e| e.name != b.p.boss).count();
        let minions0 = minions(self);
        let fin_exp = |b: &Bot| b.ours().iter().find(|u| u.general == b.p.finale_g).map_or(0, total_exp);
        let fin0 = fin_exp(self);
        let mut best: Option<(f32, crate::machine::Snapshot, String)> = None;
        for pl in plans.into_iter().take(self.p.farm_trials as usize) {
            self.m.restore(&start);
            let r = self.gplan_trial(&pl, &weak);
            // The boss attacking finale_g in the enemy phase starts the duel too
            // (虎牢關), which ends the battle: let it play out and keep it as a
            // fallback ending when no safe plan exists.
            let r = match r {
                Err(e) if self.p.boss != "華雄" && self.p.final_turn > self.turn => {
                    for _ in 0..15 {
                        self.dismiss()?;
                        self.run_ms(1900)?;
                    }
                    // The boss gone (killed: victory) also ends the battle.
                    let boss_gone = !self.enemies().iter().any(|x| x.name == self.p.boss);
                    if fin_exp(self) >= fin0 + 100 || boss_gone { Err(format!("DUEL END ({e})")) } else { Err(e) }
                }
                r => r,
            };
            let (score, desc) = match r {
                Ok(()) => {
                    let now = self.ours();
                    // A replayed (cautious) turn must leave everyone with more troops.
                    let floor = self.p.floor + 80 * self.caution as u16;
                    let safe = now.len() >= self.n_ours && now.iter().all(|u| u.troops > floor);
                    // Before kill_turn the boss lives, and (keep_alive) the last few
                    // other enemies are kept as a farm.
                    let boss_ok = self.turn >= self.p.kill_turn || (self.enemies().iter().any(|e| e.name == self.p.boss)
                        && (minions(self) >= self.p.keep_alive as usize || minions(self) >= minions0));
                    let gained = wexp(self) - exp0;
                    let sc = if safe && boss_ok { 0.0 } else { -1e6 } + 10.0 * gained
                        + 0.05 * now.iter().map(|u| u.troops as f32).sum::<f32>();
                    let plan: Vec<String> = pl.iter().map(|(g, c)| match c {
                        Some((t, tile, fire)) => format!("{g}@{tile:?}{}{t:?}", if *fire { "🔥" } else { "→" }),
                        None => format!("{g}:hold"),
                    }).collect();
                    (sc, format!("[{}] EXP(w) +{gained:.0}, {}", plan.join(" "), self.summary()))
                }
                Err(e) if e.starts_with("DUEL END") => {
                    // Before kill_turn only a fallback (no safe plan); from kill_turn
                    // on, ending the battle is the goal.
                    let gained = wexp(self) - exp0;
                    let base = if self.turn >= self.p.kill_turn { 1e5 } else { -5e5 };
                    (base + 10.0 * gained, format!("{e}: EXP(w) +{gained:.0}, {}", self.summary()))
                }
                Err(e) => (f32::NEG_INFINITY, format!("{e} in {pl:?}")),
            };
            self.note(format!("T{} plan trial: {desc} (score {score:.0})", self.turn));
            if score > f32::NEG_INFINITY && best.as_ref().map_or(true, |x| score > x.0) {
                best = Some((score, self.m.snapshot(), desc));
            }
        }
        match best {
            Some((_, snap, desc)) => {
                self.m.finish_turn(&start, &snap, self.p.live);
                if desc.starts_with("DUEL END") {
                    self.battle_over = true;
                    self.p.final_turn = self.turn;
                }
                Ok(Some(desc))
            }
            None => {
                self.m.restore(&start);
                self.m.set_trial(false);
                Ok(None)
            }
        }
    }

    fn gplan_trial(&mut self, plan: &[(u16, Option<((u8, u8), (u8, u8), bool)>)], weak: &[u16]) -> Result<(), String> {
        // Units whose target already fell to an earlier attacker rest instead.
        let mut idle: Vec<u16> = Vec::new();
        // The unit whose EXP weighs most strikes last (so it lands the kills).
        let w = self.p.exp_weight.clone();
        let mut strikes: Vec<_> = plan.iter().filter(|(_, c)| c.is_some()).collect();
        strikes.sort_by(|a, b| w.get(a.0 as usize).unwrap_or(&1.0).partial_cmp(w.get(b.0 as usize).unwrap_or(&1.0)).unwrap());
        for &&(g, c) in &strikes {
            let (target, tile, fire) = c.unwrap();
            if !self.enemies().iter().any(|e| (e.x, e.y) == target) {
                idle.push(g);
                continue;
            }
            self.select(g)?;
            let act = match self.units().into_iter().find(|u| u.general == g).and_then(|u| u.items.iter().position(|&i| i == FIRE_BOOK)) {
                Some(slot) if fire => Act::Fire(target.0, target.1, slot as u8),
                _ => Act::Attack(target.0, target.1),
            };
            self.commit(g, tile, act)?;
        }
        let mut rest: Vec<u16> = plan.iter().filter(|(_, c)| c.is_none()).map(|&(g, _)| g)
            .chain(idle).chain(weak.iter().copied()).collect();
        // Feeders (關羽, 張飛) move last: the weak have reached their spots by then.
        if self.p.feed {
            let feeders = self.p.feeders.clone();
            rest.sort_by_key(|g| feeders.contains(g));
        }
        for g in rest {
            let Some(u) = self.units().into_iter().find(|u| u.general == g) else { continue };
            let reach = self.select(g)?;
            if self.p.feed && self.p.feeders.contains(&g) {
                // Feed a 豆 to a weak ally (not a feeder) from a free tile next to it;
                // the boss's duel unit only from tiles out of everyone's reach.
                let taken: HashSet<(u8, u8)> = self.units().iter()
                    .filter(|o| o.troops > 0 && o.general != g).map(|o| (o.x, o.y)).collect();
                let slot = u.items.iter().position(|&i| i == 0x1e);
                let mut allies: Vec<Unit> = self.ours().into_iter()
                    .filter(|a| !self.p.feeders.contains(&a.general) && a.troops > 0 && a.troops < self.p.feed_below).collect();
                let shy = self.p.boss != "華雄" && g == self.p.finale_g;
                allies.sort_by_key(|a| a.troops);
                let feed = slot.and_then(|slot| allies.iter().find_map(|a| neighbours((a.x, a.y)).into_iter()
                    .filter(|n| reach.contains(n) && !taken.contains(n) && (!shy || self.threats_at(*n) == 0))
                    .min_by_key(|&n| (self.threats_at(n), n))
                    .map(|tile| (tile, (a.x, a.y), slot))));
                if let Some((tile, at, slot)) = feed {
                    self.commit(g, tile, Act::Fire(at.0, at.1, slot as u8))?;
                    self.note(format!("T{} {g} feeds a 豆 to the unit at {at:?}", self.turn));
                    continue;
                }
            }
            let taken: HashSet<(u8, u8)> = self.units().iter()
                .filter(|o| o.troops > 0 && o.general != g).map(|o| (o.x, o.y))
                .chain(self.p.avoid.iter().copied()).collect();
            if u.troops >= self.p.advance_min {
                // Healthy and idle: close in on the nearest non-boss enemy,
                // staying within reach of at most one of them.
                let mut foes: Vec<(u8, u8)> = self.enemies().iter().filter(|e| e.name != self.p.boss).map(|e| (e.x, e.y)).collect();
                // Only the boss left: wait near him but out of his reach (he hits
                // whoever stands next to him); striking him is an attack option.
                let only_boss = foes.is_empty();
                if only_boss {
                    foes = self.enemies().iter().map(|e| (e.x, e.y)).collect();
                }
                let t = reach.iter().copied().filter(|t| !taken.contains(t))
                    .min_by_key(|&t| (if only_boss { self.threats_at(t) } else { self.threats_at(t).max(1) },
                        foes.iter().map(|&f| dist(t, f)).min().unwrap_or(0), t))
                    .unwrap_or((u.x, u.y));
                self.commit(g, t, Act::Rest)?;
                continue;
            }
            if self.p.eat && u.troops < self.p.eat_below {
                if let Some(slot) = u.items.iter().position(|&i| i == 0x1e) {
                    // Eat a 豆 on the spot (the least threatened tile in reach)
                    // instead of walking to a village.
                    let t = reach.iter().copied().filter(|t| !taken.contains(t))
                        .min_by_key(|&t| (self.threats_at(t), dist(t, (u.x, u.y)), t)).unwrap_or((u.x, u.y));
                    self.commit(g, t, Act::Fire(t.0, t.1, slot as u8))?;
                    self.note(format!("T{} {g} eats a 豆 at {t:?}", self.turn));
                    continue;
                }
            }
            let free_heal: Vec<(u8, u8)> = self.p.heal.iter().copied().filter(|t| !taken.contains(t)).collect();
            let t = free_heal.iter().copied().filter(|t| reach.contains(t)).min_by_key(|&t| self.threats_at(t))
                .unwrap_or_else(|| reach.iter().copied()
                    .filter(|t| !taken.contains(t))
                    .min_by_key(|&t| (self.threats_at(t), free_heal.iter().map(|&h| dist(t, h)).min().unwrap_or(0), t))
                    .unwrap_or((u.x, u.y)));
            self.commit(g, t, Act::Rest)?;
        }
        self.end_turn()
    }

    /// Baiting turns, played instead of a normal turn when one is due:
    /// - draw: small fry gone, 李肅 / 趙岑 still at the gate → someone steps into
    ///   their attack range (out of 胡軫's and 華雄's) so they come out to fight;
    /// - lure: 李肅 and 趙岑 gone, 胡軫 not yet on the 鹿砦 → a cavalry unit (his
    ///   短兵 hits 騎兵 softly) baits him onto it, where he is farmed.
    /// Every bait unit / spot is played through the enemy phase on a snapshot and
    /// the best safe outcome is kept. Returns None when no bait turn is due.
    fn bait_turn(&mut self) -> Result<Option<String>, String> {
        if !self.p.lure {
            return Ok(None);
        }
        let en = self.enemies();
        let gate_group = ["華雄", "李肅", "胡軫", "趙岑"];
        let pair: Vec<&Unit> = en.iter().filter(|e| e.name == "李肅" || e.name == "趙岑").collect();
        let draw = !pair.is_empty()
            && en.iter().all(|e| gate_group.contains(&e.name.as_str()))
            && pair.iter().all(|e| e.x <= 6);
        let hu = en.iter().find(|e| e.name == "胡軫");
        let lure = pair.is_empty() && hu.map_or(false, |h| if self.p.lujai_ours { h.x <= 6 } else { (h.x, h.y) != LUJAI });
        if !draw && !lure {
            return Ok(None);
        }
        let targets: Vec<&str> = if draw { vec!["李肅", "趙岑"] } else { vec!["胡軫"] };
        let zone_of = |names: &[&str]| -> HashSet<(u8, u8)> {
            en.iter().filter(|e| names.contains(&e.name.as_str()))
                .filter_map(|e| self.enemy_ranges.iter().find(|(at, _)| *at == (e.x, e.y)))
                .flat_map(|(_, r)| r.iter().flat_map(|&t| neighbours(t)))
                .collect()
        };
        let zone = zone_of(&targets);
        // Tiles nobody but the targets can hit (華雄 never leaves the gate).
        let others_zone = zone_of(&gate_group.iter().copied().filter(|n| !targets.contains(n) && *n != "華雄").collect::<Vec<_>>());
        let all_zone: HashSet<(u8, u8)> = zone.union(&others_zone).copied().collect();
        let start_pos: Vec<(String, (u8, u8))> = en.iter().filter(|e| targets.contains(&e.name.as_str()))
            .map(|e| (e.name.clone(), (e.x, e.y))).collect();
        let enemy_at: HashSet<(u8, u8)> = en.iter().map(|e| (e.x, e.y)).collect();
        let mut ours = self.ours();
        ours.retain(|u| !u.acted && u.troops > 0);
        ours.sort_by_key(|u| (lure && u.class != 6, std::cmp::Reverse(u.troops)));
        let baiters: Vec<(u16, String)> = ours.iter().take(if lure { 1 } else { 3 }).map(|u| (u.general, u.name.clone())).collect();
        let all_ours: Vec<u16> = ours.iter().map(|u| u.general).collect();
        let idle = self.m.snapshot();
        self.m.set_trial(true);
        let mut best: Option<(f32, crate::machine::Snapshot, String)> = None;
        for (bait_g, bait_name) in baiters {
            self.m.restore(&idle);
            let reach = self.select(bait_g)?;
            let selected = self.m.snapshot();
            let mut spots: Vec<(u8, u8)> = zone.iter().copied()
                .filter(|t| reach.contains(t) && *t != LUJAI && !enemy_at.contains(t))
                .collect();
            spots.sort_by_key(|&t| (others_zone.contains(&t), if lure { dist(t, LUJAI) } else { 0 }, std::cmp::Reverse(t.0), t.1));
            spots.truncate(if lure { 4 } else { 3 });
            let others: Vec<u16> = all_ours.iter().copied().filter(|&g| g != bait_g).collect();
            for b in spots {
                self.m.restore(&selected);
                let r = self.bait_trial(bait_g, b, &others, &all_zone);
                let (score, desc) = match r {
                    Ok(()) => {
                        let ours = self.ours();
                        let now = self.enemies();
                        let safe = ours.len() == 3 && ours.iter().all(|u| u.troops > self.p.floor);
                        let moved: Vec<String> = start_pos.iter().map(|(n, p0)| {
                            let p = now.iter().find(|e| &e.name == n).map(|e| (e.x, e.y));
                            format!("{n} {p0:?}→{p:?}")
                        }).collect();
                        let gain = if lure && self.p.lujai_ours {
                            // Out of the gate and onto open ground is enough.
                            let p = now.iter().find(|e| e.name == "胡軫").map(|e| (e.x, e.y));
                            match p { Some(p) if p.0 >= 7 && p != LUJAI => 10000.0, Some(p) => 100.0 * p.0 as f32, None => -1e5 }
                        } else if lure {
                            let p = now.iter().find(|e| e.name == "胡軫").map(|e| (e.x, e.y));
                            if p == Some(LUJAI) { 10000.0 } else { -100.0 * p.map_or(99, |p| dist(p, LUJAI)) as f32 }
                        } else {
                            start_pos.iter().map(|(n, p0)| {
                                now.iter().find(|e| &e.name == n).map_or(0, |e| dist((e.x, e.y), *p0)) as f32 * 100.0
                            }).sum()
                        };
                        let sc = if safe { 0.0 } else { -1e6 } + gain + 0.1 * ours.iter().map(|u| u.troops as f32).sum::<f32>();
                        (sc, format!("{bait_name} baits at {b:?}: {}, {}", moved.join(", "), self.summary()))
                    }
                    Err(e) => (f32::NEG_INFINITY, format!("{bait_name} baits at {b:?}: {e}")),
                };
                self.note(format!("T{} bait trial: {desc} (score {score:.0})", self.turn));
                if score > f32::NEG_INFINITY && best.as_ref().map_or(true, |x| score > x.0) {
                    best = Some((score, self.m.snapshot(), desc));
                }
            }
        }
        match best {
            Some((_, snap, desc)) => {
                self.m.finish_turn(&idle, &snap, self.p.live);
                Ok(Some(desc))
            }
            None => {
                self.m.restore(&idle);
                self.m.set_trial(false);
                Ok(None)
            }
        }
    }

    /// One bait attempt: bait to `b`, the others out of `zone`, end the turn.
    fn bait_trial(&mut self, bait: u16, b: (u8, u8), others: &[u16], zone: &HashSet<(u8, u8)>) -> Result<(), String> {
        self.commit_rest_or_bean(bait, b)?;
        for &g in others {
            let Some(u) = self.units().into_iter().find(|u| u.general == g) else { continue };
            let reach = self.select(g)?;
            let taken: HashSet<(u8, u8)> = self.units().iter()
                .filter(|o| o.troops > 0 && o.general != g).map(|o| (o.x, o.y)).collect();
            let t = reach.iter().copied()
                .filter(|t| !zone.contains(t) && !taken.contains(t) && *t != LUJAI)
                .min_by_key(|&t| (self.threats_at(t), dist(t, LUJAI), t))
                .unwrap_or((u.x, u.y));
            self.commit_rest_or_bean(g, t)?;
        }
        self.end_turn()
    }

    pub fn play(&mut self) -> Result<i32, String> {
        if let Some(p) = self.p.resume.clone() {
            self.resume_from(&p)?;
        } else {
            self.boot()?;
            self.start_exp = self.ours().iter().map(total_exp).sum();
        }
        let start: i32 = self.start_exp;
        let order = [0u16, 2, 1];
        // Turn-start states, so a hopeless turn can go back and replay the one before
        // it more carefully (min_caution raises the starting caution of that turn).
        let mut starts: std::collections::HashMap<u32, (crate::machine::Snapshot, Vec<(u8, u8)>, usize)> = Default::default();
        let mut min_caution: std::collections::HashMap<u32, u32> = Default::default();
        let first_turn = self.turn;
        let mut backtracks = 0;
        while self.turn <= self.p.final_turn.min(self.p.stop_turn) {
            if !starts.contains_key(&self.turn) {
                starts.insert(self.turn, (self.m.snapshot(), self.tents_left.clone(), self.log.len()));
            }
            let mut ok = false;
            let c0 = *min_caution.get(&self.turn).unwrap_or(&0);
            // Everyone on our side at the start of the turn must still be there after it.
            self.n_ours = self.ours().iter().filter(|u| u.troops > 0).count().max(3);
            for attempt in c0..=3 {
                let heal = self.p.heal_below;
                self.caution = attempt;
                if attempt > 0 {
                    self.p.heal_below = heal + 150 * attempt as u16;
                }
                let last = self.turn >= self.p.final_turn;
                if last && attempt == c0 && self.p.liu_finale {
                    self.finale_kill = self.trial_liubei_kill()?;
                    self.note(format!("T{} 劉備 can last-hit 華雄: {}", self.turn, self.finale_kill));
                }
                self.scan_enemies()?;
                // Only the boss left: nothing more to farm safely, finish now if
                // finale_g can reach him (otherwise play on and close in).
                // (Not before boss_wake: until then the boss stays put and is farmed.)
                let only_boss = self.p.boss != "華雄" && self.turn >= self.p.boss_wake
                    && self.enemies().iter().all(|e| e.name == self.p.boss);
                if (last || only_boss) && self.p.boss != "華雄" {
                    // Any other map: `finale_g` attacks the boss first, from a free tile next to
                    // him (on 虎牢關 that starts the duel, which ends the battle).
                    let g = self.p.finale_g;
                    let r = (|| -> Result<(), String> {
                        let boss = self.enemies().into_iter().find(|e| e.name == self.p.boss).ok_or("boss not on the map")?;
                        let reach = self.select(g)?;
                        let taken: HashSet<(u8, u8)> = self.units().iter()
                            .filter(|o| o.troops > 0 && o.general != g).map(|o| (o.x, o.y)).collect();
                        let tile = neighbours((boss.x, boss.y)).into_iter()
                            .filter(|n| reach.contains(n) && !taken.contains(n)).min()
                            .ok_or("no free tile next to the boss in reach")?;
                        self.commit(g, tile, Act::Attack(boss.x, boss.y))
                    })();
                    self.note(format!("T{} {g} → {}: {r:?}", self.turn, self.p.boss));
                    if r.is_ok() || last {
                        self.p.heal_below = heal;
                        self.p.final_turn = self.turn;
                        ok = true;
                        break;
                    }
                    self.ensure_idle()?;
                }
                if !last && (only_boss || self.turn + 2 >= self.p.final_turn) {
                    self.guan_to_gate()?;
                }
                let lured = if last { None } else if let Some(d) = self.farm_turn()? { Some(d) }
                    else if let Some(d) = self.hua_turn()? { Some(d) }
                    else if let Some(d) = self.gplan_turn()? { Some(d) } else { self.bait_turn()? };
                if let Some(d) = &lured {
                    self.note(format!("T{} bait: {d}", self.turn));
                }
                for &g in &order {
                    if lured.is_some() || (last && g == if self.finale_kill { 0 } else { 1 }) {
                        continue;
                    }
                    let d = self.play_unit(g)?;
                    self.note(format!("T{} {d}", self.turn));
                }
                if last && self.finale_kill {
                    // 劉備 last-hits 華雄 from the gate: the boss kill ends the battle.
                    let r = self.select(0).and_then(|_| self.commit(0, HUAXIONG_GATE, Act::Attack(3, 9)));
                    self.note(format!("T{} 劉備 → 華雄 last hit: {r:?}", self.turn));
                    self.p.heal_below = heal;
                    ok = true;
                    break;
                }
                if last {
                    // 關羽 attacks 華雄 from the gate: the duel ends the battle.
                    let r = self.select(1).and_then(|_| self.commit(1, HUAXIONG_GATE, Act::Attack(3, 9)));
                    self.note(format!("T{} 關羽 → 華雄 duel: {r:?}", self.turn));
                    self.p.heal_below = heal;
                    ok = true;
                    break;
                }
                if lured.is_none() {
                    self.end_turn()?;
                }
                self.p.heal_below = heal;
                let ours = self.ours();
                let floor = self.p.floor;
                if self.battle_over || (ours.len() >= self.n_ours && ours.iter().all(|u| u.troops > floor)) {
                    ok = true;
                    break;
                }
                self.note(format!("T{} outcome unsafe ({}); replaying the turn with caution {}", self.turn, self.summary(), attempt + 1));
                let (snap, tents, loglen) = starts.get(&self.turn).unwrap();
                self.m.restore(snap);
                self.tents_left = tents.clone();
                self.log.truncate(*loglen);
            }
            self.caution = 0;
            if !ok {
                // Go back to the latest earlier turn that can still be played more
                // carefully (turns already at maximum caution are skipped).
                let mut back = self.turn;
                while back > first_turn && *min_caution.get(&(back - 1)).unwrap_or(&0) >= 3 {
                    back -= 1;
                }
                if back > first_turn && backtracks < 12 {
                    backtracks += 1;
                    for t in back..=self.turn {
                        if t != back - 1 {
                            starts.remove(&t);
                        }
                    }
                    for t in back..=self.turn {
                        min_caution.remove(&t);
                    }
                    self.turn = back - 1;
                    let c = min_caution.entry(self.turn).or_insert(0);
                    *c = (*c + 1).min(3);
                    let c = *c;
                    let (snap, tents, loglen) = starts.get(&self.turn).unwrap();
                    self.m.restore(snap);
                    self.tents_left = tents.clone();
                    self.log.truncate(*loglen);
                    self.note(format!("T{} could not be saved; back to T{} with caution {c}", self.turn + 1, self.turn));
                    continue;
                }
                return Err(format!("could not keep everyone safe on turn {}", self.turn));
            }
            let foes: Vec<String> = self.enemies().iter()
                .filter(|e| FARM.contains(&e.name.as_str()) || e.name == "李肅")
                .map(|e| format!("{}({},{}) {}", e.name, e.x, e.y, e.troops)).collect();
            self.note(format!("T{} done: {} | {}", self.turn, self.summary(), foes.join(", ")));
            if self.turn % 5 == 0 {
                self.shot(&format!("turn{:02}", self.turn))?;
            }
            self.save_turn()?;
            self.turn += 1;
        }
        let end: i32 = self.ours().iter().map(total_exp).sum();
        self.note(format!("final (before the duel settles): {} total gained {}", self.summary(), end - start));
        if self.p.stop_turn < self.p.final_turn {
            self.shot("end")?;
            return Ok(end - start);
        }
        // Let the duel / victory play out and read the general table afterwards.
        for i in 0..30 {
            self.dismiss()?;
            self.run_ms(1900)?;
            if i % 5 == 4 {
                self.note(format!("after the finale ({}s): {}", (i + 1) * 2, self.summary()));
            }
        }
        self.shot("end")?;
        // The state after the battle (town / next scene), to go on from.
        self.m.save_state(&self.out.join("state_end.bin"))?;
        Ok(end - start)
    }

    /// Outside battle (town, story, route choice): load `state`, then play `steps`
    /// ("x,y" = click, "Nx,y" = click N times, "r:x,y" = right click, "k:NAME" =
    /// key, ';'-separated), a screenshot after each step; saves walk_end.bin.
    pub fn walk(&mut self, state: &PathBuf, steps: &str) -> Result<(), String> {
        self.m.load_state(state)?;
        for (i, st) in steps.split(';').filter(|t| !t.is_empty()).enumerate() {
            if let Some(k) = st.strip_prefix("k:") {
                self.input(&format!("key {k}"))?;
                self.run_ms(1500)?;
            } else {
                let (rc, xy) = match st.strip_prefix("r:") { Some(r) => (true, r), None => (false, st) };
                let (n, xy) = match xy.split_once('x') { Some((n, r)) if !n.contains(',') => (n.parse().unwrap_or(1), r), _ => (1, xy) };
                let (x, y) = xy.split_once(',').ok_or("step x,y")?;
                let (x, y): (i32, i32) = (x.parse().map_err(|_| "x")?, y.parse().map_err(|_| "y")?);
                for _ in 0..n {
                    if rc { self.input(&format!("rclick {x} {y}"))?; self.run_ms(1500)?; } else { self.click(x, y, 1500)?; }
                }
            }
            self.settle(2000)?;
            std::fs::write(self.out.join(format!("walk_{i:02}.png")), self.m.screenshot_png()).map_err(|e| e.to_string())?;
        }
        self.m.save_state(&self.out.join("walk_end.bin"))
    }

    pub fn write_outputs(&self, result: &str) -> Result<(), String> {
        let j = self.m.journal_entries().join("\n");
        std::fs::write(self.out.join("journal.txt"), format!("# eiketsu-rt autoplay journal (ips/ms={IPS_PER_MS})\n{j}\n")).map_err(|e| e.to_string())?;
        std::fs::write(self.out.join("log.txt"), self.log.join("\n")).map_err(|e| e.to_string())?;
        std::fs::write(self.out.join("result.txt"), format!("{result}\nparams: {:?}\n", self.p)).map_err(|e| e.to_string())?;
        Ok(())
    }
}
