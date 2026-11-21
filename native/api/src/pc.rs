//! The Nest PC (GDD 2.3 "switching is free at any Nest's PC terminal (once per 10 minutes, not in
//! combat)", 3.8 "the replaced move can be relearned at any Nest PC for free").
//!
//! Opened with `/pc` or F at a PC terminal, standing inside a Nest (or within 2.5 b of its
//! terminal). The client draws a Pokémon-PC style box grid of the player's unlocked species;
//! arrows/WASD move, Enter/F/Space opens that Pokémon's summary (its Switch, moves, stats and
//! Prestige), M the quick Moves list, R restores the kit's moves, X/Backspace closes (Esc is only
//! the pause menu).
//!
//! Wire (channel `capipimon:play`):
//! - server → client: `pc\t<cooldown secs>\t<current species or ->\t<species,species,...>[\t<level,level,...>]`
//!   (open or refresh; each one's own level, in the same order), `pc\t-` (close), `pcnote\t<text>` (a line under the grid).
//! - client → server: `pc\topen`, `pc\tpick\t<species>`, `pc\trelearn`, `pc\tclose`,
//!   `pc\tmoves`, `pc\tteach\t<move id>\t<slot 0-3>` (0: LMB); server → client
//!   `pcslots\t<q>\t<e>\t<r>\t<lmb>`, and `moveinfo\t<id>|<area>|<effects>\t...` for the info
//!   under the Moves list (the Move Tutor's too).
//! - Summary (play-test #32, Gen 3 style INFO / STATS / MOVES; Enter on a box slot): client →
//!   server `pc\tsum\t<species>`, `pc\tset\t<species>\t<move id>\t<slot 0-3>`,
//!   `pc\tprestige\t<species>`, `pc\tnature\t<species>\t<nature>` (STATS: any nature the player has
//!   unlocked, free); server → client `moveinfo` for its moves, then `pcsum\t...`
//!   (`Summary::line`). Older servers drop the new verbs (the page shows what the client knows);
//!   older clients ignore `pcsum`.

use super::*;

// The server half: private, never published (docs/CLIENT_SERVER_SPLIT.md).
#[cfg(feature = "capipimon_server")]
include!("server/pc.rs");

// ---------------------------------------------------------------- client

struct View {
    cool_until: Instant,
    current: String,
    list: Vec<String>,
    /// Each one's own level, in `list`'s order (none from older servers).
    levels: Vec<u32>,
    sel: usize,
    /// The move list is showing instead of the box.
    moves: Option<MoveList>,
    /// One Pokémon's summary is showing instead of the box.
    sum: Option<SumView>,
}

// ---------------------------------------------------------------- summary (#32)

/// One Pokémon's summary, as the server sends it (`pcsum`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    pub species: String,
    pub level: u32,
    /// XP into the level, and what the level needs (0 at the cap).
    pub xp: u64,
    pub need: u64,
    pub nature: String,
    /// The stats the nature raises and lowers (1 Atk .. 5 Spe; the same twice: neutral).
    pub up: usize,
    pub down: usize,
    /// HP, Atk, Def, SpA, SpD, Spe.
    pub ivs: [u8; 6],
    pub stats: [u32; 6],
    /// The held item's name; empty for none.
    pub held: String,
    /// Its line's prestige tier, and whether Prestige can be done (level 100, seasons on).
    pub tier: u32,
    pub ready: bool,
    /// Capipidex marks (`dex::SEEN` | `PLAYED` | `EVOLVED` | `MASTERED`).
    pub marks: u8,
    /// The move ids on LMB, Q, E, R.
    pub slots: [String; 4],
    /// The move ids it can have (the Moves list's and the Tutor's).
    pub options: Vec<String>,
    /// The natures the player has unlocked, each with the stats it raises and lowers (as `up` and
    /// `down`), for the STATS page's nature change. Empty from servers before it.
    pub natures: Vec<(String, usize, usize)>,
}

/// The `pcsum` layout's version: fields are only ever added at the end.
const SUM_V: &str = "1";

fn clean(s: &str) -> String {
    s.replace(['\t', ','], " ")
}

impl Summary {
    pub fn line(&self) -> String {
        let csv = |v: Vec<String>| v.join(",");
        format!(
            "pcsum\t{SUM_V}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            clean(&self.species),
            self.level,
            self.xp,
            self.need,
            clean(&self.nature),
            self.up,
            self.down,
            csv(self.ivs.iter().map(u8::to_string).collect()),
            csv(self.stats.iter().map(u32::to_string).collect()),
            if self.held.is_empty() { "-".to_string() } else { clean(&self.held) },
            self.tier,
            u8::from(self.ready),
            self.marks,
            csv(self.slots.iter().map(|s| if s.is_empty() { "-".to_string() } else { clean(s) }).collect()),
            csv(self.options.iter().map(|s| clean(s)).collect()),
            csv(self.natures.iter().map(|(n, up, down)| format!("{}:{up}:{down}", clean(n).replace(':', " "))).collect()),
        )
    }

    /// A `pcsum` line's fields; None if it isn't one or is cut short.
    pub fn parse(f: &[&str]) -> Option<Summary> {
        let ["pcsum", _v, species, level, xp, need, nature, up, down, ivs, stats, held, tier, ready, marks, slots, options, more @ ..] = f else { return None };
        // Added later at the end: the unlocked natures, `Name:up:down` each.
        let natures = more.first().map_or_else(Vec::new, |s| {
            s.split(',')
                .filter_map(|e| {
                    let mut p = e.split(':');
                    let (n, up, down) = (p.next()?, p.next()?.parse().ok()?, p.next()?.parse().ok()?);
                    (!n.is_empty() && up < 6 && down < 6).then(|| (n.to_string(), up, down))
                })
                .collect()
        });
        fn six<T: std::str::FromStr>(s: &str) -> Option<[T; 6]> {
            let v: Vec<T> = s.split(',').map(|x| x.parse().ok()).collect::<Option<_>>()?;
            v.try_into().ok()
        }
        let slots: Vec<String> = slots.split(',').map(|s| if s == "-" { String::new() } else { s.to_string() }).collect();
        Some(Summary {
            species: species.to_string(),
            level: level.parse().ok()?,
            xp: xp.parse().ok()?,
            need: need.parse().ok()?,
            nature: nature.to_string(),
            up: up.parse().ok()?,
            down: down.parse().ok()?,
            ivs: six(ivs)?,
            stats: six(stats)?,
            held: if *held == "-" { String::new() } else { held.to_string() },
            tier: tier.parse().ok()?,
            ready: *ready == "1",
            marks: marks.parse().ok()?,
            slots: slots.try_into().ok()?,
            options: options.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect(),
            natures,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Info,
    Stats,
    Moves,
}

const PAGES: [Page; 3] = [Page::Info, Page::Stats, Page::Moves];
const SLOT_KEYS: [&str; 4] = ["LMB", "Q", "E", "R"];

/// The move slot a number key puts a move on, in GDD 3.1 order: [1] LMB (slot 0), [2] Q, [3] E,
/// [4] R. Slot numbers are what saves and the server use, so they never change meaning.
pub(super) fn slot_for_key(key: &str) -> Option<usize> {
    match key {
        "key.keyboard.1" => Some(0),
        "key.keyboard.2" => Some(1),
        "key.keyboard.3" => Some(2),
        "key.keyboard.4" => Some(3),
        _ => None,
    }
}
/// A second P within this long prestiges.
const PRESTIGE_CONFIRM_SECS: f32 = 3.0;

struct SumView {
    species: String,
    page: Page,
    /// None until the server's answer (an older server never sends one).
    data: Option<Summary>,
    /// MOVES: the slot picked (0 LMB .. 3 R), and the option highlighted while choosing its move.
    slot: usize,
    pick: Option<usize>,
    /// STATS: the unlocked nature highlighted while choosing one (an index into `Summary::natures`).
    nature: Option<usize>,
    /// P was pressed once: a second press in time prestiges.
    confirm: Option<Instant>,
}

impl SumView {
    fn new(species: &str) -> Self {
        SumView { species: species.to_string(), page: Page::Info, data: None, slot: 0, pick: None, nature: None, confirm: None }
    }

    /// Prestige can be done from here: the server says it's ready, and this is the Pokémon in use.
    fn can_prestige(&self, current: &str) -> bool {
        self.species == current && self.data.as_ref().is_some_and(|d| d.ready)
    }

    /// STATS can change the nature: the player has unlocked another one.
    fn can_change_nature(&self) -> bool {
        self.data.as_ref().is_some_and(|d| d.natures.iter().any(|n| !n.0.eq_ignore_ascii_case(&d.nature)))
    }

    /// The stats raised and lowered as the page shows them: the highlighted nature's while choosing.
    fn shown_nature(&self) -> Option<(usize, usize)> {
        let d = self.data.as_ref()?;
        Some(match self.nature.and_then(|i| d.natures.get(i)) {
            Some(n) => (n.1, n.2),
            None => (d.up, d.down),
        })
    }

    fn confirming(&self) -> bool {
        self.confirm.is_some_and(|at| at.elapsed().as_secs_f32() < PRESTIGE_CONFIRM_SECS)
    }
}

#[derive(Clone, Debug, PartialEq)]
enum SumAct {
    Stay,
    /// Back to the box.
    Back,
    Send(String),
}

/// A key in the summary (`current`: the species in use): what it does. Every key is taken.
fn sum_key(s: &mut SumView, current: &str, key: &str, now: Instant) -> SumAct {
    const UP: [&str; 2] = ["key.keyboard.w", "key.keyboard.up"];
    const DOWN: [&str; 2] = ["key.keyboard.s", "key.keyboard.down"];
    const OK: [&str; 4] = ["key.keyboard.enter", "key.keyboard.keypad.enter", "key.keyboard.space", "key.keyboard.f"];
    const BACK: [&str; 2] = ["key.keyboard.x", "key.keyboard.backspace"];
    let armed = s.confirm.take().filter(|at| now.duration_since(*at).as_secs_f32() < PRESTIGE_CONFIRM_SECS);
    // Choosing a nature (Space and Enter: F is for the world's own interactions).
    if let Some(i) = s.nature {
        const ACCEPT: [&str; 3] = ["key.keyboard.enter", "key.keyboard.keypad.enter", "key.keyboard.space"];
        let natures = s.data.as_ref().map_or(&[][..], |d| d.natures.as_slice());
        let n = natures.len().max(1);
        if UP.contains(&key) {
            s.nature = Some((i + n - 1) % n);
        } else if DOWN.contains(&key) {
            s.nature = Some((i + 1) % n);
        } else if ACCEPT.contains(&key) {
            let pick = natures.get(i).map(|n| n.0.clone());
            let now = s.data.as_ref().map(|d| d.nature.clone()).unwrap_or_default();
            s.nature = None;
            if let Some(name) = pick.filter(|p| !p.eq_ignore_ascii_case(&now)) {
                return SumAct::Send(format!("pc\tnature\t{}\t{name}", s.species));
            }
        } else if BACK.contains(&key) {
            s.nature = None;
        }
        return SumAct::Stay;
    }
    // Choosing a move for a slot.
    if let Some(i) = s.pick {
        let options = s.data.as_ref().map_or(&[][..], |d| d.options.as_slice());
        let n = options.len().max(1);
        if UP.contains(&key) {
            s.pick = Some((i + n - 1) % n);
        } else if DOWN.contains(&key) {
            s.pick = Some((i + 1) % n);
        } else if OK.contains(&key) {
            let id = options.get(i).cloned();
            s.pick = None;
            if let Some(id) = id {
                return SumAct::Send(format!("pc\tset\t{}\t{id}\t{}", s.species, s.slot));
            }
        } else if BACK.contains(&key) {
            s.pick = None;
        }
        return SumAct::Stay;
    }
    let page = PAGES.iter().position(|p| *p == s.page).unwrap_or(0);
    match key {
        "key.keyboard.a" | "key.keyboard.left" => s.page = PAGES[(page + PAGES.len() - 1) % PAGES.len()],
        "key.keyboard.d" | "key.keyboard.right" => s.page = PAGES[(page + 1) % PAGES.len()],
        k if BACK.contains(&k) => return SumAct::Back,
        k if s.page == Page::Info && OK.contains(&k) && s.species != current => return SumAct::Send(format!("pc\tpick\t{}", s.species)),
        "key.keyboard.p" if s.page == Page::Info && s.can_prestige(current) => {
            if armed.is_some() {
                return SumAct::Send(format!("pc\tprestige\t{}", s.species));
            }
            s.confirm = Some(now);
        }
        "key.keyboard.space" | "key.keyboard.enter" | "key.keyboard.keypad.enter" if s.page == Page::Stats && s.can_change_nature() => {
            // Starts on the nature it has now.
            let d = s.data.as_ref().expect("can_change_nature");
            s.nature = Some(d.natures.iter().position(|n| n.0.eq_ignore_ascii_case(&d.nature)).unwrap_or(0));
        }
        k if s.page == Page::Moves && UP.contains(&k) => s.slot = (s.slot + 3) % 4,
        k if s.page == Page::Moves && DOWN.contains(&k) => s.slot = (s.slot + 1) % 4,
        k if s.page == Page::Moves && OK.contains(&k) => {
            // Starts on the move in that slot now (nothing to pick before the server's answer).
            if let Some(d) = s.data.as_ref().filter(|d| !d.options.is_empty()) {
                s.pick = Some(d.options.iter().position(|o| *o == d.slots[s.slot]).unwrap_or(0));
            }
        }
        _ => {}
    }
    SumAct::Stay
}

/// A move id as the summary shows it: the species' own kit move, else the learnable move.
fn move_of(species: &str, id: &str) -> Option<Move> {
    kit(species).and_then(|k| k.moves.iter().find(|m| m.id == id)).or_else(|| learn_move(id)).cloned()
}

struct MoveList {
    options: Vec<Move>,
    sel: usize,
}

#[derive(Default)]
pub struct PcClient {
    view: Option<View>,
    note: Option<(String, Instant)>,
    /// The Q/E/R moves now.
    slots: [String; 3],
    /// The LMB move now (servers before LMB swapping don't send it).
    lmb: Option<String>,
}

/// `moveinfo`: each move's area and effects, as the server works them out (by move id).
static MOVE_INFO: LazyLock<Mutex<HashMap<String, (String, String)>>> = LazyLock::new(Default::default);

pub static PC_CLIENT: LazyLock<Mutex<PcClient>> = LazyLock::new(Default::default);

const COLS: usize = 6;
const ROWS: usize = 5;
/// One box: 6 x 5. A collection bigger than one box spills into BOX 2, 3... (Onii, 2026-11-20).
pub const BOX: usize = COLS * ROWS;

/// How many boxes `n` Pokémon fill (at least one).
pub fn boxes(n: usize) -> usize {
    n.div_ceil(BOX).max(1)
}

/// Where a box-grid key moves the cursor: within the box, and off its left, right, top or bottom
/// edge into the box before or after (wrapping round the last box), so every box is reachable.
pub fn grid_step(sel: usize, n: usize, key: &str) -> usize {
    let count = boxes(n);
    let (bx, i) = (sel / BOX, sel % BOX);
    let (col, row) = (i % COLS, i / COLS);
    let prev = (bx + count - 1) % count;
    let next = (bx + 1) % count;
    let to = match key {
        "key.keyboard.left" | "key.keyboard.a" if col == 0 => prev * BOX + row * COLS + COLS - 1,
        "key.keyboard.left" | "key.keyboard.a" => sel - 1,
        "key.keyboard.right" | "key.keyboard.d" if col == COLS - 1 => next * BOX + row * COLS,
        "key.keyboard.right" | "key.keyboard.d" => sel + 1,
        "key.keyboard.up" | "key.keyboard.w" if row == 0 => prev * BOX + (ROWS - 1) * COLS + col,
        "key.keyboard.up" | "key.keyboard.w" => sel - COLS,
        "key.keyboard.down" | "key.keyboard.s" if row == ROWS - 1 => next * BOX + col,
        "key.keyboard.down" | "key.keyboard.s" => sel + COLS,
        // Whole boxes at a time.
        "key.keyboard.page.up" | "key.keyboard.z" => prev * BOX + i,
        "key.keyboard.page.down" | "key.keyboard.c" => next * BOX + i,
        _ => sel,
    };
    // An empty slot in the last box: its last Pokémon.
    to.min(n.saturating_sub(1))
}

/// A `pc`/`pcnote` message; true when it was one.
pub fn heard(f: &[&str]) -> bool {
    let mut c = PC_CLIENT.lock().unwrap();
    let mut opened = false;
    match f {
        ["pc", "-", ..] => c.view = None,
        // A late refresh of a PC this client just closed for another menu.
        ["pc", ..] if c.view.is_none() && !menus::server_may_open(menus::Menu::Pc) => {}
        ["pc", cool, current, list, more @ ..] => {
            opened = c.view.is_none();
            let secs = cool.parse::<u64>().unwrap_or(0);
            let list: Vec<String> = list.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect();
            let levels: Vec<u32> = more.first().map_or_else(Vec::new, |l| l.split(',').filter_map(|n| n.parse().ok()).collect());
            let sel = c.view.as_ref().map_or(0, |v| v.sel).min(list.len().saturating_sub(1));
            // A refresh keeps an open summary while its Pokémon is still in the box.
            let sum = c.view.as_mut().and_then(|v| v.sum.take()).filter(|s| list.contains(&s.species));
            c.view = Some(View { cool_until: Instant::now() + std::time::Duration::from_secs(secs), current: current.to_string(), list, levels, sel, moves: None, sum });
            c.note = None;
        }
        ["pcsum", ..] => {
            let Some(new) = Summary::parse(f) else { return true };
            if let Some(s) = c.view.as_mut().and_then(|v| v.sum.as_mut()).filter(|s| s.species == new.species) {
                s.data = Some(new);
                s.pick = None;
                s.nature = None;
            }
        }
        ["pcslots", q, e, r, more @ ..] => {
            c.slots = [q.to_string(), e.to_string(), r.to_string()];
            c.lmb = more.first().map(|l| l.to_string());
        }
        ["moveinfo", rows @ ..] => {
            let mut info = MOVE_INFO.lock().unwrap();
            for row in rows {
                let mut p = row.splitn(3, '|');
                if let (Some(id), Some(area), Some(fx)) = (p.next(), p.next(), p.next()) {
                    info.insert(id.to_string(), (area.to_string(), fx.to_string()));
                }
            }
        }
        ["pcnote", text, ..] => c.note = Some((text.to_string(), Instant::now())),
        _ => return false,
    }
    drop(c);
    if opened {
        menus::opening(menus::Menu::Pc);
    }
    true
}

/// Bought Move Tutor moves (`tutor`'s `tutored` line): added to an open Moves list.
pub fn add_moves(moves: &[Move]) {
    let mut c = PC_CLIENT.lock().unwrap();
    if let Some(m) = c.view.as_mut().and_then(|v| v.moves.as_mut()) {
        for mv in moves {
            if !m.options.iter().any(|o| o.id == mv.id) {
                m.options.push(mv.clone());
            }
        }
    }
}

pub fn forget() {
    *PC_CLIENT.lock().unwrap() = PcClient::default();
}

pub fn is_open() -> bool {
    PC_CLIENT.lock().unwrap().view.is_some()
}

/// Closes it (another menu is opening); true when it was open.
pub fn shut() -> bool {
    PC_CLIENT.lock().unwrap().view.take().is_some()
}

/// Keys while the PC is open (all of them are taken, so moves don't fire).
pub fn key(host: &mut dyn Host, key: &str) -> bool {
    let mut c = PC_CLIENT.lock().unwrap();
    let Some(v) = c.view.as_mut() else { return false };
    let n = v.list.len().max(1);
    let mut send: Option<String> = None;
    if let Some(s) = v.sum.as_mut() {
        match sum_key(s, &v.current, key, Instant::now()) {
            SumAct::Stay => {}
            SumAct::Back => v.sum = None,
            SumAct::Send(line) => send = Some(line),
        }
        drop(c);
        if let Some(s) = send {
            let _ = host.net_send_to_server(CHANNEL, s.as_bytes());
        }
        return true;
    }
    if let Some(m) = v.moves.as_mut() {
        let len = m.options.len().max(1);
        match key {
            "key.keyboard.up" | "key.keyboard.w" => m.sel = (m.sel + len - 1) % len,
            "key.keyboard.down" | "key.keyboard.s" => m.sel = (m.sel + 1) % len,
            "key.keyboard.1" | "key.keyboard.2" | "key.keyboard.3" | "key.keyboard.4" => {
                if let (Some(o), Some(slot)) = (m.options.get(m.sel), slot_for_key(key)) {
                    send = Some(format!("pc\tteach\t{}\t{slot}", o.id));
                }
            }
            "key.keyboard.m" | "key.keyboard.x" | "key.keyboard.backspace" => v.moves = None,
            _ => {}
        }
        drop(c);
        if let Some(s) = send {
            let _ = host.net_send_to_server(CHANNEL, s.as_bytes());
        }
        return true;
    }
    match key {
        "key.keyboard.m" => {
            v.moves = Some(MoveList { options: progress::teachable(&v.current, progress::client_level()), sel: 0 });
            send = Some("pc\tmoves".to_string());
        }
        "key.keyboard.left" | "key.keyboard.a" | "key.keyboard.right" | "key.keyboard.d" | "key.keyboard.up" | "key.keyboard.w" | "key.keyboard.down" | "key.keyboard.s" | "key.keyboard.page.up" | "key.keyboard.page.down" | "key.keyboard.z" | "key.keyboard.c" => {
            v.sel = grid_step(v.sel, v.list.len(), key);
        }
        "key.keyboard.enter" | "key.keyboard.f" | "key.keyboard.space" | "key.keyboard.keypad.enter" => {
            if let Some(s) = v.list.get(v.sel) {
                v.sum = Some(SumView::new(s));
                send = Some(format!("pc\tsum\t{s}"));
            }
        }
        "key.keyboard.r" => send = Some("pc\trelearn".to_string()),
        "key.keyboard.x" | "key.keyboard.backspace" | "key.keyboard.p" | "key.keyboard.e" => {
            c.view = None;
            drop(c);
            let _ = host.net_send_to_server(CHANNEL, b"pc\tclose");
            return true;
        }
        _ => {}
    }
    if v.sel >= n {
        v.sel = n - 1;
    }
    drop(c);
    if let Some(s) = send {
        let _ = host.net_send_to_server(CHANNEL, s.as_bytes());
    }
    true
}

// Colours: the classic storage-system look (blue frame, light wallpaper, white slots).
const PC_FRAME: u32 = 0xF02A_4F8C;
const PC_HEAD: u32 = 0xFF3D_7BD9;
const PC_WALL: u32 = 0xF0A8_D8F0;
const PC_WALL_DOT: u32 = 0xF094_C8E8;
const PC_SLOT: u32 = 0xE6F4_F8FC;
const PC_SLOT_EDGE: u32 = 0xFF7F_A8D0;
const PC_SEL: u32 = 0xFFFF_D23F;

/// Height of the move info under a list.
pub(super) const INFO_H: f32 = 36.0;

fn secs(x: f32) -> String {
    let s = format!("{x:.1}");
    format!("{} s", s.strip_suffix(".0").unwrap_or(&s))
}

/// The selected move's numbers, under a menu's list: label then value, nothing that isn't there.
pub(super) fn info_hud(host: &mut dyn Host, b: &mut HudBatch, x: f32, y: f32, w: f32, mv: &Move, label: u32, value: u32) {
    let mut first: Vec<(&str, String)> = Vec::new();
    if mv.support.is_none() && mv.power > 0.0 {
        first.push(("Power", format!("{}", mv.power as u32)));
    }
    first.push(("PP", format!("{}", mv.energy as u32)));
    first.push(("Cooldown", secs(mv.cooldown)));
    let (area, fx) = MOVE_INFO.lock().unwrap().get(&mv.id).cloned().unwrap_or_default();
    let mut tx = x;
    for (l, v) in &first {
        host.text(b, l, [tx, y], label, true);
        tx += host.text_width(l) + 4.0;
        host.text(b, v, [tx, y], value, true);
        tx += host.text_width(v) + 12.0;
    }
    let mut ly = y + 11.0;
    for (l, v) in [("Area", area), ("Effects", fx)] {
        if v.is_empty() {
            continue;
        }
        host.text(b, l, [x, ly], label, true);
        let lw = host.text_width(l) + 4.0;
        host.text(b, &fit(host, &v, w - lw), [x + lw, ly], value, true);
        ly += 11.0;
    }
}

/// The move list: everything this Pokémon can have on LMB/Q/E/R, nine rows at a time.
fn moves_hud(host: &mut dyn Host, b: &mut HudBatch, m: &MoveList, slots: &[String; 3], lmb: Option<&String>, note: Option<&(String, Instant)>) {
    const SHOWN: usize = 9;
    let (w, h) = (290.0f32, 158.0f32 + INFO_H);
    let (x, y) = ((b.width - w) / 2.0, (b.height - h) / 2.0);
    pill(host, b, x, y, w, h, PC_FRAME);
    pill(host, b, x + 3.0, y + 3.0, w - 6.0, 14.0, PC_HEAD);
    host.text(b, "Moves", [x + 8.0, y + 6.0], 0xFFFFFF, true);
    let bar = format!("{}Q {}   E {}   R {}", lmb.map_or(String::new(), |l| format!("LMB {l}   ")), slots[0], slots[1], slots[2]);
    host.text(b, &fit(host, &bar, w - 14.0), [x + 7.0, y + 21.0], 0xFFE9A0, true);
    let start = m.sel.saturating_sub(SHOWN / 2).min(m.options.len().saturating_sub(SHOWN));
    for (i, o) in m.options.iter().enumerate().skip(start).take(SHOWN) {
        let ry = y + 33.0 + (i - start) as f32 * 11.0;
        let selected = i == m.sel;
        rect(host, b, x + 5.0, ry - 1.0, w - 10.0, 10.0, if selected { PC_SEL } else { PC_SLOT });
        rect(host, b, x + 8.0, ry + 2.0, 4.0, 4.0, 0xFF00_0000 | type_color(&o.kind));
        host.text(b, &fit(host, &o.name, 130.0), [x + 15.0, ry], 0x202830, false);
        let stats = if o.support.is_some() { format!("support  {} PP", o.energy as u32) } else { format!("{} pow  {} PP", o.power as u32, o.energy as u32) };
        let sw = host.text_width(&stats);
        host.text(b, &stats, [x + w - 9.0 - sw, ry], 0x404A58, false);
    }
    if let Some(o) = m.options.get(m.sel) {
        info_hud(host, b, x + 8.0, y + 134.0, w - 16.0, o, 0xA8C0E0, 0xFFFFFF);
    }
    guide::keyed_centred(host, b, "[W] [S] choose  put on: [1] LMB  [2] Q  [3] E  [4] R  [X] back", x + w / 2.0, y + h - 22.0, 0xD8E4F4);
    if let Some((t, at)) = note {
        if at.elapsed().as_secs_f32() < 4.0 {
            let tw = host.text_width(t);
            host.text(b, t, [x + (w - tw) / 2.0, y + h - 11.0], 0xFFE9A0, true);
        }
    }
}

const STAT_SHORT: [&str; 6] = ["HP", "Atk", "Def", "SpA", "SpD", "Spe"];
const LABEL: u32 = 0xA8C0E0;

/// One label: value row of a summary page.
fn sum_row(host: &mut dyn Host, b: &mut HudBatch, x: f32, y: f32, w: f32, label: &str, value: &str) {
    host.text(b, label, [x, y], LABEL, true);
    host.text(b, &fit(host, value, w - 52.0), [x + 52.0, y], 0xFFFFFF, true);
}

/// A move as one line of a list: type, name, numbers on the right.
fn move_row(host: &mut dyn Host, b: &mut HudBatch, x: f32, y: f32, w: f32, mv: Option<&Move>, id: &str, selected: bool) {
    rect(host, b, x, y - 1.0, w, 10.0, if selected { PC_SEL } else { PC_SLOT });
    let Some(mv) = mv else {
        host.text(b, &fit(host, id, w - 12.0), [x + 10.0, y], 0x202830, false);
        return;
    };
    rect(host, b, x + 3.0, y + 2.0, 4.0, 4.0, 0xFF00_0000 | type_color(&mv.kind));
    let stats = if mv.support.is_some() || mv.power <= 0.0 { format!("{} PP", mv.energy as u32) } else { format!("{} pow  {} PP", mv.power as u32, mv.energy as u32) };
    let sw = host.text_width(&stats);
    host.text(b, &fit(host, &mv.name, w - sw - 18.0), [x + 10.0, y], 0x202830, false);
    host.text(b, &stats, [x + w - 4.0 - sw, y], 0x404A58, false);
}

/// The summary of one Pokémon (play-test #32): INFO, STATS and MOVES, A/D between them.
fn summary_hud(host: &mut dyn Host, b: &mut HudBatch, s: &SumView, current: &str, level: Option<u32>, note: Option<&(String, Instant)>) {
    let (w, h) = (300.0f32, 186.0f32);
    let (x, y) = ((b.width - w) / 2.0, (b.height - h) / 2.0);
    pill(host, b, x, y, w, h, PC_FRAME);
    pill(host, b, x + 3.0, y + 3.0, w - 6.0, 14.0, PC_HEAD);
    let k = kit(&s.species);
    let d = s.data.as_ref();
    // Page tabs, right of the header.
    let mut tx = x + w - 5.0;
    for p in PAGES.iter().rev() {
        let label = match p {
            Page::Info => "INFO",
            Page::Stats => "STATS",
            Page::Moves => "MOVES",
        };
        let tw = host.text_width(label);
        tx -= tw + 8.0;
        if *p == s.page {
            pill(host, b, tx, y + 4.0, tw + 6.0, 12.0, PC_SEL);
            host.text(b, label, [tx + 3.0, y + 6.0], 0x203048, false);
        } else {
            host.text(b, label, [tx + 3.0, y + 6.0], 0xD8E4F4, true);
        }
    }
    let name = k.map_or(s.species.clone(), |k| k.name.clone());
    let title = match d.map(|d| d.level).or(level) {
        Some(l) => format!("{name}  Lv {l}"),
        None => name,
    };
    emblem(host, b, x + 11.0, y + 10.0, 4.5);
    host.text(b, &fit(host, &title, tx - x - 24.0), [x + 19.0, y + 6.0], 0xFFFFFF, true);
    // Left: the Pokémon itself (`portraits`, rendered from its model) and its types under it.
    let (lx, ly, lw, bh) = (x + 6.0, y + 21.0, 78.0f32, h - 47.0);
    rect(host, b, lx, ly, lw, bh, PC_WALL);
    let types = k.map_or(&[][..], |k| k.types.as_slice());
    let pic = 72.0f32;
    let drawn = crate::portraits::body(host, b, &s.species, [lx + (lw - pic) / 2.0, ly + 2.0, pic, pic]);
    if !drawn {
        let col = types.first().map_or(0xA0A0A0, |t| type_color(t));
        rect(host, b, lx + lw / 2.0 - 14.0, ly + 8.0, 28.0, 28.0, 0xFF00_0000 | col);
        if let Some(t2) = types.get(1) {
            rect(host, b, lx + lw / 2.0, ly + 8.0, 14.0, 28.0, 0xFF00_0000 | type_color(t2));
        }
    }
    let first_type = if drawn { ly + 78.0 } else { ly + 44.0 };
    for (i, t) in types.iter().enumerate() {
        let label = t.to_uppercase();
        let tw = host.text_width(&label);
        let py = first_type + i as f32 * 13.0;
        pill(host, b, lx + (lw - tw - 6.0) / 2.0, py, tw + 6.0, 10.0, 0xFF00_0000 | type_color(t));
        host.text(b, &label, [lx + (lw - tw) / 2.0, py + 1.0], 0xFFFFFF, true);
    }
    if s.species == current {
        emblem(host, b, lx + lw - 8.0, ly + 7.0, 3.5);
        let tw = host.text_width("In use");
        host.text(b, "In use", [lx + (lw - tw) / 2.0, ly + bh - 12.0], 0x203048, false);
    }
    // Right: the page.
    let (rx, ry, rw) = (lx + lw + 6.0, ly, w - lw - 18.0);
    pill(host, b, rx, ry, rw, bh, 0xF018_2A48);
    let hint: String;
    match s.page {
        Page::Info => {
            let (cx, cw) = (rx + 6.0, rw - 12.0);
            match d {
                Some(d) => {
                    let mut rows: Vec<(&str, String)> = vec![("Level", d.level.to_string())];
                    rows.push(("To next", if d.need == 0 { "Max".to_string() } else { format!("{} XP", d.need.saturating_sub(d.xp)) }));
                    let nat = if d.up != d.down && d.up < 6 && d.down < 6 { format!("{}  +{} -{}", d.nature, STAT_SHORT[d.up], STAT_SHORT[d.down]) } else { d.nature.clone() };
                    rows.push(("Nature", nat));
                    rows.push(("Held", if d.held.is_empty() { "None".to_string() } else { d.held.clone() }));
                    let tier = if d.tier == 0 { "None".to_string() } else { format!("P{} {}", d.tier, "\u{2605}".repeat(d.tier.min(5) as usize)) };
                    rows.push(("Prestige", if s.can_prestige(current) { format!("{tier}  Ready") } else { tier }));
                    let marks: Vec<&str> = [(dex::SEEN, "Seen"), (dex::PLAYED, "Played"), (dex::EVOLVED, "Evolved"), (dex::MASTERED, "Mastered")].into_iter().filter(|(bit, _)| d.marks & bit != 0).map(|(_, n)| n).collect();
                    if !marks.is_empty() {
                        rows.push(("Dex", marks.join(", ")));
                    }
                    for (i, (l, v)) in rows.iter().enumerate() {
                        sum_row(host, b, cx, ry + 6.0 + i as f32 * 13.0, cw, l, v);
                    }
                }
                None => {
                    if let Some(l) = level {
                        sum_row(host, b, cx, ry + 6.0, cw, "Level", &l.to_string());
                    }
                    host.text(b, "Loading...", [rx + 6.0, ry + 19.0], LABEL, true);
                }
            }
            let mut keys = vec!["[A] [D] Page"];
            if s.species != current {
                keys.push("[Enter] Switch");
            }
            if s.can_prestige(current) {
                keys.push("[P] Prestige");
            }
            keys.push("[X] Back");
            hint = keys.join("  ");
        }
        Page::Stats => {
            let (up, down) = s.shown_nature().unwrap_or((0, 0));
            match d {
                Some(d) => {
                    host.text(b, "Stat", [rx + 6.0, ry + 5.0], LABEL, true);
                    host.text(b, "IV", [rx + 92.0, ry + 5.0], LABEL, true);
                    for i in 0..6 {
                        let ty = ry + 19.0 + i as f32 * 13.0;
                        let (label, colour) = match i {
                            _ if up == down => (STAT_SHORT[i].to_string(), 0xFFFFFF),
                            _ if i == up => (format!("{}+", STAT_SHORT[i]), 0xFF9A8A),
                            _ if i == down => (format!("{}-", STAT_SHORT[i]), 0x8CB8FF),
                            _ => (STAT_SHORT[i].to_string(), 0xFFFFFF),
                        };
                        host.text(b, &label, [rx + 6.0, ty], colour, true);
                        let v = d.stats[i].to_string();
                        host.text(b, &v, [rx + 78.0 - host.text_width(&v), ty], 0xFFFFFF, true);
                        let iv = d.ivs[i].min(31);
                        host.text(b, &iv.to_string(), [rx + 92.0, ty], 0xFFFFFF, true);
                        let (bx, bw) = (rx + 108.0, rw - 116.0);
                        pill(host, b, bx, ty + 1.0, bw, 6.0, 0xFF26_2626);
                        rect(host, b, bx + 1.0, ty + 2.0, (bw - 2.0) * f32::from(iv) / 31.0, 4.0, if iv == 31 { 0xFFFF_D23F } else { 0xFF3F_A9F5 });
                    }
                    // The nature, or the one highlighted while choosing (W/S through the unlocked ones).
                    let ny = ry + 19.0 + 6.0 * 13.0 + 6.0;
                    host.text(b, "Nature", [rx + 6.0, ny], LABEL, true);
                    let (name, picking) = match s.nature.and_then(|i| d.natures.get(i).map(|n| (i, n))) {
                        Some((i, n)) => (format!("\u{25B2} {} \u{25BC}  {}/{}", n.0, i + 1, d.natures.len()), true),
                        None => (d.nature.clone(), false),
                    };
                    if picking {
                        pill(host, b, rx + 48.0, ny - 2.0, rw - 54.0, 12.0, PC_SEL);
                        host.text(b, &name, [rx + 52.0, ny], 0x203048, false);
                    } else {
                        host.text(b, &name, [rx + 52.0, ny], 0xFFFFFF, true);
                    }
                }
                None => {
                    host.text(b, "Loading...", [rx + 6.0, ry + 6.0], LABEL, true);
                }
            }
            hint = if s.nature.is_some() {
                "[W] [S] Choose  [Space] Change  [X] Back".to_string()
            } else if s.can_change_nature() {
                "[A] [D] Page  [Space] Change nature  [X] Back".to_string()
            } else {
                "[A] [D] Page  [X] Back".to_string()
            };
        }
        Page::Moves => {
            let (cx, cw) = (rx + 5.0, rw - 10.0);
            match (d, s.pick) {
                (Some(d), Some(p)) => {
                    host.text(b, &format!("Put on {}:", SLOT_KEYS[s.slot.min(3)]), [cx + 1.0, ry + 4.0], 0xFFE9A0, true);
                    const SHOWN: usize = 7;
                    let start = p.saturating_sub(SHOWN / 2).min(d.options.len().saturating_sub(SHOWN));
                    for (i, id) in d.options.iter().enumerate().skip(start).take(SHOWN) {
                        move_row(host, b, cx, ry + 16.0 + (i - start) as f32 * 11.0, cw, move_of(&s.species, id).as_ref(), id, i == p);
                    }
                    if let Some(mv) = d.options.get(p).and_then(|id| move_of(&s.species, id)) {
                        info_hud(host, b, cx + 1.0, ry + 16.0 + SHOWN as f32 * 11.0 + 3.0, cw - 2.0, &mv, LABEL, 0xFFFFFF);
                    }
                    hint = "[W] [S] Choose  [Enter] Put on  [X] Back".to_string();
                }
                _ => {
                    // Before the server's answer: the kit's own moves.
                    let ids: [String; 4] = match d {
                        Some(d) => d.slots.clone(),
                        None => std::array::from_fn(|i| k.and_then(|k| k.moves.get(i)).map_or_else(String::new, |m| m.id.clone())),
                    };
                    for (i, id) in ids.iter().enumerate() {
                        let ty = ry + 6.0 + i as f32 * 14.0;
                        pill(host, b, cx, ty - 1.0, 22.0, 10.0, PC_HEAD);
                        let kw = host.text_width(SLOT_KEYS[i]);
                        host.text(b, SLOT_KEYS[i], [cx + 11.0 - kw / 2.0, ty], 0xFFFFFF, true);
                        move_row(host, b, cx + 25.0, ty, cw - 25.0, move_of(&s.species, id).as_ref(), if id.is_empty() { "-" } else { id.as_str() }, i == s.slot);
                    }
                    if let Some(mv) = ids.get(s.slot).and_then(|id| move_of(&s.species, id)) {
                        info_hud(host, b, cx + 1.0, ry + 64.0, cw - 2.0, &mv, LABEL, 0xFFFFFF);
                    }
                    if d.is_none() {
                        host.text(b, "Loading...", [rx + 6.0, ry + 104.0], LABEL, true);
                    }
                    hint = "[A] [D] Page  [W] [S] Slot  [Enter] Change  [X] Back".to_string();
                }
            }
        }
    }
    guide::keyed_centred(host, b, &hint, x + w / 2.0, y + h - 22.0, 0xD8E4F4);
    let line = if s.confirming() { Some("Back to Lv 1, keeps everything. [P] again".to_string()) } else { note.filter(|(_, at)| at.elapsed().as_secs_f32() < 4.0).map(|(t, _)| t.clone()) };
    if let Some(t) = line {
        guide::keyed_centred(host, b, &t, x + w / 2.0, y + h - 11.0, 0xFFE9A0);
    }
}

pub fn hud(host: &mut dyn Host, b: &mut HudBatch) {
    let c = PC_CLIENT.lock().unwrap();
    let Some(v) = c.view.as_ref() else { return };
    if let Some(s) = &v.sum {
        let level = v.list.iter().position(|x| *x == s.species).and_then(|i| v.levels.get(i)).copied();
        return summary_hud(host, b, s, &v.current, level, c.note.as_ref());
    }
    if let Some(m) = &v.moves {
        return moves_hud(host, b, m, &c.slots, c.lmb.as_ref(), c.note.as_ref());
    }
    let (cw, ch) = (34.0f32, 28.0f32);
    let (gw, gh) = (COLS as f32 * cw, ROWS as f32 * ch);
    let side = 118.0f32;
    let (w, h) = (gw + side + 18.0, gh + 44.0);
    let (x, y) = ((b.width - w) / 2.0, (b.height - h) / 2.0);
    pill(host, b, x, y, w, h, PC_FRAME);
    pill(host, b, x + 3.0, y + 3.0, w - 6.0, 14.0, PC_HEAD);
    emblem(host, b, x + 11.0, y + 10.0, 4.5);
    let (bx, count) = (v.sel / BOX, boxes(v.list.len()));
    let title = if count > 1 { format!("Capipimon PC  \u{2022}  \u{25C0} BOX {} of {count} \u{25B6}", bx + 1) } else { "Capipimon PC  \u{2022}  BOX 1".to_string() };
    host.text(b, &title, [x + 19.0, y + 6.0], 0xFFFFFF, true);
    let cool = v.cool_until.saturating_duration_since(Instant::now()).as_secs();
    let status = if cool > 0 { format!("Switch in {}:{:02}", cool / 60, cool % 60) } else { "Switch ready".to_string() };
    let sw = host.text_width(&status);
    host.text(b, &status, [x + w - 6.0 - sw, y + 6.0], if cool > 0 { 0xFFD0A0 } else { 0xB8F5C0 }, true);
    // The box: wallpaper with a dot pattern, then 6 x 5 slots.
    let (gx, gy) = (x + 6.0, y + 21.0);
    rect(host, b, gx, gy, gw, gh, PC_WALL);
    for i in 0..(COLS * 2) {
        for j in 0..(ROWS * 2) {
            if (i + j) % 2 == 0 {
                rect(host, b, gx + i as f32 * cw / 2.0 + 7.0, gy + j as f32 * ch / 2.0 + 6.0, 2.0, 2.0, PC_WALL_DOT);
            }
        }
    }
    for slot in 0..BOX {
        let i = bx * BOX + slot;
        let (sx, sy) = (gx + (slot % COLS) as f32 * cw + 2.0, gy + (slot / COLS) as f32 * ch + 2.0);
        let sel = i == v.sel;
        pill(host, b, sx - 1.0, sy - 1.0, cw - 2.0, ch - 2.0, if sel { PC_SEL } else { PC_SLOT_EDGE });
        pill(host, b, sx, sy, cw - 4.0, ch - 4.0, PC_SLOT);
        let Some(s) = v.list.get(i) else { continue };
        let k = kit(s);
        // Its head (`portraits`), else a type-coloured block; the name under it.
        if !crate::portraits::head(host, b, s, [sx + (cw - 4.0) / 2.0 - 7.0, sy, 14.0, 14.0]) {
            let col = k.and_then(|k| k.types.first()).map_or(0xA0A0A0, |t| type_color(t));
            rect(host, b, sx + cw / 2.0 - 7.0, sy + 3.0, 10.0, 10.0, 0xFF00_0000 | col);
            if let Some(t2) = k.and_then(|k| k.types.get(1)) {
                rect(host, b, sx + cw / 2.0 - 2.0, sy + 3.0, 5.0, 10.0, 0xFF00_0000 | type_color(t2));
            }
        }
        if *s == v.current {
            emblem(host, b, sx + cw - 9.0, sy + 5.0, 3.0);
        }
        let label = k.map_or(s.clone(), |k| k.name.clone());
        let fitted = fit(host, &label, cw - 5.0);
        let lw = host.text_width(&fitted);
        host.text(b, &fitted, [sx + (cw - 4.0 - lw) / 2.0, sy + 15.0], 0x203048, false);
    }
    // The side panel: the selected Pokémon.
    let (px, py) = (gx + gw + 6.0, gy);
    pill(host, b, px, py, side, gh, 0xF018_2A48);
    if let Some(k) = v.list.get(v.sel).and_then(|s| kit(s)) {
        // Its own level, from servers that send it.
        let lv = v.levels.get(v.sel).map_or_else(String::new, |n| format!("Lv {n}"));
        let lw = if lv.is_empty() { 0.0 } else { host.text_width(&lv) + 4.0 };
        host.text(b, &fit(host, &k.name, side - 8.0 - lw), [px + 4.0, py + 4.0], 0xFFFFFF, true);
        if !lv.is_empty() {
            host.text(b, &lv, [px + side - 4.0 - (lw - 4.0), py + 4.0], 0xFFE9A0, true);
        }
        let mut tx = px + 4.0;
        for t in &k.types {
            let label = t.to_uppercase();
            let tw = host.text_width(&label);
            pill(host, b, tx, py + 15.0, tw + 6.0, 10.0, 0xFF00_0000 | type_color(t));
            host.text(b, &label, [tx + 3.0, py + 16.0], 0xFFFFFF, true);
            tx += tw + 9.0;
        }
        let bars = [("HP", k.base[0], 0xFF4C_D964u32), ("Atk", k.base[1].max(k.base[3]), 0xFFFF_8A3D), ("Def", k.base[2].max(k.base[4]), 0xFFE0_C040), ("Spe", k.base[5], 0xFF3F_A9F5)];
        for (i, (label, val, col)) in bars.iter().enumerate() {
            let by = py + 30.0 + i as f32 * 10.0;
            host.text(b, label, [px + 4.0, by], 0xC0C8D8, true);
            pill(host, b, px + 26.0, by + 1.0, side - 32.0, 6.0, 0xFF26_2626);
            rect(host, b, px + 27.0, by + 2.0, (side - 34.0) * (val / 150.0).clamp(0.0, 1.0), 4.0, *col);
        }
        for (i, m) in k.moves.iter().take(4).enumerate() {
            let my = py + 74.0 + i as f32 * 9.0;
            rect(host, b, px + 4.0, my + 2.0, 4.0, 4.0, 0xFF00_0000 | type_color(&m.kind));
            host.text(b, &fit(host, &m.name, side - 14.0), [px + 11.0, my], 0xFFFFFF, true);
        }
        let tag = if v.list.get(v.sel) == Some(&v.current) { "In use" } else { "[Enter] Summary" };
        guide::keyed_centred(host, b, tag, px + side / 2.0, py + gh - 11.0, 0xFFE9A0);
    }
    let keys = if count > 1 { "[WASD] Move  [Z] [C] Box  [Enter] Summary  [M] Moves  [R] Reset  [X] Close" } else { "[WASD] Move  [Enter] Summary  [M] Moves  [R] Reset moves  [X] Close" };
    guide::keyed_centred(host, b, keys, x + w / 2.0, y + h - 19.0, 0xD8E4F4);
    if v.list.len() <= 1 {
        let lines = ["Only one Pokémon so far.", "KO wilds to research more."];
        let tw = lines.iter().map(|l| host.text_width(l)).fold(0.0, f32::max);
        let (cx, ty) = (gx + gw / 2.0, gy + gh / 2.0);
        rect(host, b, cx - tw / 2.0 - 6.0, ty - 3.0, tw + 12.0, 26.0, 0xE018_2A4A);
        for (i, l) in lines.iter().enumerate() {
            let lw = host.text_width(l);
            host.text(b, l, [cx - lw / 2.0, ty + 1.0 + i as f32 * 11.0], 0xFFFFFF, true);
        }
    }
    if let Some((t, at)) = &c.note {
        if at.elapsed().as_secs_f32() < 4.0 {
            let tw = host.text_width(t);
            host.text(b, t, [x + (w - tw) / 2.0, y + h - 9.0], 0xFFE9A0, true);
        }
    }
}

#[cfg(test)]
mod summary_client_tests {
    use super::*;

    #[test]
    fn a_full_box_spills_into_more_boxes_and_the_cursor_scrolls_through_them() {
        assert_eq!((boxes(0), boxes(30), boxes(31), boxes(65)), (1, 1, 2, 3));
        // 40 Pokémon: BOX 1 full, BOX 2 has 10.
        let n = 40;
        assert_eq!(grid_step(5, n, "key.keyboard.d"), 30, "off the right edge into BOX 2");
        assert_eq!(grid_step(30, n, "key.keyboard.a"), 5, "and back");
        assert_eq!(grid_step(24, n, "key.keyboard.s"), 30, "off the bottom into BOX 2's top row");
        assert_eq!(grid_step(0, n, "key.keyboard.w"), 39, "off the top of BOX 1: BOX 2's last row, clamped to its last Pokemon");
        assert_eq!(grid_step(3, n, "key.keyboard.c"), 33, "a whole box on");
        assert_eq!(grid_step(33, n, "key.keyboard.c"), 3, "wraps round");
        assert_eq!(grid_step(7, n, "key.keyboard.d"), 8);
        assert_eq!(grid_step(7, n, "key.keyboard.s"), 13);
        // One box: the old wrapping inside it.
        assert_eq!(grid_step(0, 12, "key.keyboard.a"), 5);
        assert_eq!(grid_step(5, 12, "key.keyboard.d"), 0);
    }
    use std::time::Duration;

    /// Number keys follow the hotbar (GDD 3.1): [1] LMB, [2] Q, [3] E, [4] R; the slot numbers
    /// saves use (0 = LMB) stay as they were, so nobody's moves move.
    #[test]
    fn number_keys_put_moves_in_hotbar_order() {
        let got: Vec<_> = ["1", "2", "3", "4", "5", "q"].iter().map(|k| slot_for_key(&format!("key.keyboard.{k}"))).collect();
        assert_eq!(got, [Some(0), Some(1), Some(2), Some(3), None, None]);
        let keys: Vec<_> = (1..=4).map(|n| SLOT_KEYS[slot_for_key(&format!("key.keyboard.{n}")).unwrap()]).collect();
        assert_eq!(keys, ["LMB", "Q", "E", "R"]);
    }

    const LINE: &str = "pcsum\t1\teevee\t12\t50\t280\tTimid\t5\t1\t1,2,3,4,5,31\t40,20,22,24,26,30\t-\t1\t1\t3\ttackle,quickattack,-,bite\ttackle,quickattack,bite,swift\textra";

    #[test]
    fn a_pcsum_line_from_the_server_reads() {
        let f: Vec<&str> = LINE.split('\t').collect();
        let s = Summary::parse(&f).expect("parses");
        assert_eq!((s.species.as_str(), s.level, s.xp, s.need), ("eevee", 12, 50, 280));
        assert_eq!((s.nature.as_str(), s.up, s.down), ("Timid", 5, 1));
        assert_eq!(s.ivs, [1, 2, 3, 4, 5, 31]);
        assert_eq!(s.stats[5], 30);
        assert_eq!((s.held.as_str(), s.tier, s.ready, s.marks), ("", 1, true, 3));
        assert_eq!(s.slots, ["tackle".to_string(), "quickattack".to_string(), String::new(), "bite".to_string()]);
        assert_eq!(s.options.len(), 4);
        let bad = LINE.replace("1,2,3,4,5,31", "1,2,3");
        assert_eq!(Summary::parse(&bad.split('\t').collect::<Vec<_>>()), None, "five IVs short");
    }

    fn with_data(species: &str, ready: bool) -> SumView {
        let mut s = SumView::new(species);
        s.data = Some(Summary {
            species: species.to_string(),
            ready,
            slots: ["tackle".to_string(), "quickattack".to_string(), "bite".to_string(), "swift".to_string()],
            options: vec!["tackle".to_string(), "quickattack".to_string(), "bite".to_string(), "swift".to_string(), "dig".to_string()],
            ..Default::default()
        });
        s
    }

    fn press(s: &mut SumView, current: &str, keys: &[&str], now: Instant) -> Vec<SumAct> {
        keys.iter().map(|k| sum_key(s, current, &format!("key.keyboard.{k}"), now)).collect()
    }

    #[test]
    fn pages_turn_with_a_and_d_and_x_goes_back() {
        let mut s = SumView::new("eevee");
        let now = Instant::now();
        press(&mut s, "eevee", &["d"], now);
        assert_eq!(s.page, Page::Stats);
        press(&mut s, "eevee", &["d", "d"], now);
        assert_eq!(s.page, Page::Info, "wraps");
        press(&mut s, "eevee", &["a"], now);
        assert_eq!(s.page, Page::Moves);
        assert_eq!(press(&mut s, "eevee", &["x"], now), [SumAct::Back]);
    }

    #[test]
    fn enter_on_info_switches_to_another_pokemon_only() {
        let now = Instant::now();
        let mut s = SumView::new("eevee");
        assert_eq!(press(&mut s, "pikachu", &["enter"], now), [SumAct::Send("pc\tpick\teevee".to_string())]);
        assert_eq!(press(&mut s, "eevee", &["enter"], now), [SumAct::Stay], "already in use");
    }

    #[test]
    fn prestige_takes_two_presses_in_time() {
        let now = Instant::now();
        let prestige = SumAct::Send("pc\tprestige\teevee".to_string());
        // Not ready, or not the one in use: P does nothing.
        let mut s = with_data("eevee", false);
        assert_eq!(press(&mut s, "eevee", &["p", "p", "p"], now), [SumAct::Stay, SumAct::Stay, SumAct::Stay]);
        let mut s = with_data("eevee", true);
        assert_eq!(press(&mut s, "pikachu", &["p", "p"], now), [SumAct::Stay, SumAct::Stay]);
        // Ready and in use: the second press sends it.
        assert_eq!(press(&mut s, "eevee", &["p", "p"], now), [SumAct::Stay, prestige.clone()]);
        // Too slow: the second press only arms it again.
        press(&mut s, "eevee", &["p"], now);
        let late = now + Duration::from_secs_f32(PRESTIGE_CONFIRM_SECS + 0.5);
        assert_eq!(press(&mut s, "eevee", &["p"], late), [SumAct::Stay]);
        assert_eq!(press(&mut s, "eevee", &["p"], late), [prestige]);
        // Another key in between disarms it.
        assert_eq!(press(&mut s, "eevee", &["p", "w", "p"], now), [SumAct::Stay, SumAct::Stay, SumAct::Stay]);
    }

    #[test]
    fn a_move_is_changed_from_the_moves_page() {
        let now = Instant::now();
        // Before the server's answer there is nothing to choose from.
        let mut s = SumView::new("eevee");
        s.page = Page::Moves;
        press(&mut s, "eevee", &["enter"], now);
        assert_eq!(s.pick, None);
        let mut s = with_data("eevee", false);
        s.page = Page::Moves;
        press(&mut s, "eevee", &["s", "s"], now);
        assert_eq!(s.slot, 2, "E");
        press(&mut s, "eevee", &["enter"], now);
        assert_eq!(s.pick, Some(2), "starts on the move in that slot (bite)");
        let acts = press(&mut s, "eevee", &["s", "s", "enter"], now);
        assert_eq!(acts.last(), Some(&SumAct::Send("pc\tset\teevee\tdig\t2".to_string())));
        assert_eq!(s.pick, None);
        // X while choosing backs out of the list only.
        press(&mut s, "eevee", &["w", "enter"], now);
        assert_eq!(s.slot, 1);
        assert_eq!(press(&mut s, "eevee", &["x"], now), [SumAct::Stay]);
        assert_eq!((s.pick, s.page), (None, Page::Moves));
        // W/S on other pages don't move the slot.
        s.page = Page::Stats;
        press(&mut s, "eevee", &["s"], now);
        assert_eq!(s.slot, 1);
    }

    #[test]
    fn the_nature_is_changed_from_the_stats_page() {
        let now = Instant::now();
        let mut s = with_data("eevee", false);
        s.page = Page::Stats;
        // Only its own nature unlocked: nothing to change to.
        let d = s.data.as_mut().unwrap();
        (d.nature, d.up, d.down) = ("Hardy".to_string(), 1, 1);
        s.data.as_mut().unwrap().natures = vec![("Hardy".to_string(), 1, 1)];
        press(&mut s, "eevee", &["space"], now);
        assert_eq!(s.nature, None);
        s.data.as_mut().unwrap().natures = vec![("Hardy".to_string(), 1, 1), ("Adamant".to_string(), 1, 3), ("Timid".to_string(), 5, 1)];
        // F is the world's: it doesn't open the list.
        press(&mut s, "eevee", &["f"], now);
        assert_eq!(s.nature, None);
        press(&mut s, "eevee", &["space"], now);
        assert_eq!(s.nature, Some(0), "starts on the one it has");
        assert_eq!(s.shown_nature(), Some((1, 1)));
        press(&mut s, "eevee", &["s"], now);
        assert_eq!(s.shown_nature(), Some((1, 3)), "the colours follow the highlighted one");
        press(&mut s, "eevee", &["w", "w"], now);
        assert_eq!(s.nature, Some(2), "wraps");
        // X backs out of the list only; the page shows its own nature again.
        assert_eq!(press(&mut s, "eevee", &["x"], now), [SumAct::Stay]);
        assert_eq!((s.nature, s.page, s.shown_nature()), (None, Page::Stats, Some((1, 1))));
        // Space on the one it has already changes nothing.
        assert_eq!(press(&mut s, "eevee", &["space", "space"], now), [SumAct::Stay, SumAct::Stay]);
        assert_eq!(press(&mut s, "eevee", &["space", "s", "s", "enter"], now).last(), Some(&SumAct::Send("pc\tnature\teevee\tTimid".to_string())));
        assert_eq!(s.nature, None);
        // A/D don't turn the page while choosing.
        press(&mut s, "eevee", &["space", "d"], now);
        assert_eq!(s.page, Page::Stats);
    }

    #[test]
    fn the_summary_survives_a_box_refresh_and_takes_its_data() {
        let _g = menus::test_lock();
        forget();
        menus::asked(menus::Menu::Pc);
        assert!(heard(&["pc", "0", "pikachu", "pikachu,eevee"]));
        PC_CLIENT.lock().unwrap().view.as_mut().unwrap().sum = Some(SumView::new("eevee"));
        let f: Vec<&str> = LINE.split('\t').collect();
        assert!(heard(&f));
        let has = || PC_CLIENT.lock().unwrap().view.as_ref().and_then(|v| v.sum.as_ref()).map(|s| s.data.as_ref().map(|d| d.level));
        assert_eq!(has(), Some(Some(12)));
        // Another Pokémon's summary arriving late doesn't land on this one.
        let other = LINE.replace("\teevee\t12\t", "\tpikachu\t30\t");
        assert!(heard(&other.split('\t').collect::<Vec<_>>()));
        assert_eq!(has(), Some(Some(12)));
        assert!(heard(&["pc", "0", "pikachu", "pikachu,eevee"]));
        assert_eq!(has(), Some(Some(12)), "kept across a refresh");
        assert!(heard(&["pc", "0", "pikachu", "pikachu"]));
        assert_eq!(has(), None, "gone with its Pokémon");
        forget();
    }
}
