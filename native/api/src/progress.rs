//! Persistent progression (GDD 4.1 to 4.5, 3.8): account level and XP, evolution, daily bonus.
//!
//! Saved per player in `capipimon-players.json` (`level`, `xp` into the level, `last_day`,
//! `streak`, `research`). A player's Pokémon plays at `max(its level, saved level)`, so a test
//! `/capi become <species> 50` isn't pulled down by a level-up.
//!
//! XP in: PvP KO (60 x LevelMod, upset bounty, 25% on a repeat victim within 3 min), the survival
//! tick (5 a minute outside Nests), wild NPC KOs (`6 + 1.5 x level`, PvE diminishing returns),
//! the daily login bonus. Multipliers: catch-up (`clamp(1 + 0.2 x (median - level), 1, 2)`),
//! caught boost x1.5 (`caught_boost`), capped at x3 together.
//!
//! Evolution (GDD 3.8): when the level reaches a stage's level (`data/evolutions.json`, made by
//! `tools/evolutions.py`), the player gets a prompt; V starts a 3 s channel (1 s in a Nest) that
//! losing HP cancels. Players can ignore it and stay as they are.
//!
//! A branch (Eevee, Kirlia, Wurmple...) with more than one form ready: V opens a numbered list and
//! a number key picks. A single form evolves on V, as before.
//!
//! Wire: server → client `xp\t<level>\t<xp>\t<need>\t<gain>\t<why>`, `evo\t<species,species... or ->`;
//! client → server `evolve[\t<species>]`.

use super::*;
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};

// The server half: private, never published (docs/CLIENT_SERVER_SPLIT.md).
#[cfg(feature = "capipimon_server")]
include!("server/progress.rs");

/// A learnset move the game can actually play as a Q/E/R move. Free basics stay on LMB.
pub fn playable(id: &str) -> Option<&'static Move> {
    learn_move(id).filter(|m| m.energy > 0.0 && !m.filler)
}

/// Learnset moves of `species` that unlock after level `after`, up to and including `to`.
fn unlocked(species: &str, after: u32, to: u32) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (at, id) in EVOS.get(species).map_or(&[][..], |e| &e.learn) {
        if *at > after && *at <= to && playable(id).is_some() && !out.contains(id) {
            out.push(id.clone());
        }
    }
    out
}

// The server half of the public file of the same name, which includes it: private, never published
// (docs/CLIENT_SERVER_SPLIT.md). Its items are that file's own, so they see everything it has.

#[derive(Deserialize)]
struct Evo {
    base: String,
    evo: Vec<(String, u32)>,
    learn: Vec<(u32, String)>,
}

static EVOS: LazyLock<HashMap<String, Evo>> =
    LazyLock::new(|| serde_json::from_str(include_str!("../data/evolutions.json")).unwrap_or_default());

/// What the Nest PC offers: the species' own Q/E/R kit moves, then its learnset so far.
pub fn teachable(species: &str, level: u32) -> Vec<Move> {
    let mut out: Vec<Move> = kit(species).map_or_else(Vec::new, |k| k.moves.iter().skip(1).take(3).cloned().collect());
    for id in unlocked(species, 0, level) {
        if !out.iter().any(|m| m.id == id) {
            out.extend(playable(&id).cloned());
        }
    }
    out
}

// ---------------------------------------------------------------- client

#[derive(Default)]
pub struct ProgClient {
    xp: Option<(u32, u64, u64)>,
    gain: Option<(u64, String, Instant)>,
    evo: Vec<String>,
    /// The numbered list is open.
    picking: bool,
    /// The XP multiplier and the server's median level, for the HUD (GDD 11.4).
    mult: f32,
    median: u32,
    /// F went down at the evolve prompt: when, and whether the hold has fired.
    f_held: Option<(Instant, bool)>,
    /// A move on offer: id, name, and the Q/E/R moves it could replace.
    learn: Option<(String, String, [String; 3])>,
    /// The replace list is open.
    learn_open: bool,
}

pub static PROG_CLIENT: LazyLock<Mutex<ProgClient>> = LazyLock::new(Default::default);

pub fn heard(f: &[&str]) -> bool {
    let mut c = PROG_CLIENT.lock().unwrap();
    match f {
        ["xp", lvl, xp, need, gain, why, more @ ..] => {
            c.mult = more.get(1).and_then(|r| r.parse().ok()).unwrap_or(1.0);
            c.median = more.get(2).and_then(|r| r.parse().ok()).unwrap_or(0);
            c.xp = Some((lvl.parse().unwrap_or(1), xp.parse().unwrap_or(0), need.parse().unwrap_or(60)));
            let g: u64 = gain.parse().unwrap_or(0);
            if g > 0 {
                c.gain = Some((g, why.to_string(), Instant::now()));
            }
        }
        ["learnoffer", "-", ..] => {
            c.learn = None;
            c.learn_open = false;
        }
        ["learnoffer", id, name, q, e, r, ..] => {
            c.learn = Some((id.to_string(), name.to_string(), [q.to_string(), e.to_string(), r.to_string()]));
            c.learn_open = false;
        }
        ["evo", "-", ..] => {
            c.evo.clear();
            c.picking = false;
        }
        ["evo", to, ..] => {
            c.evo = to.split(',').map(str::to_string).collect();
            c.picking = false;
        }
        _ => return false,
    }
    true
}

pub fn forget() {
    *PROG_CLIENT.lock().unwrap() = ProgClient::default();
}

pub fn key(host: &mut dyn Host, key: &str) -> bool {
    let mut c = PROG_CLIENT.lock().unwrap();
    if key == "key.keyboard.n" && c.learn.is_some() {
        c.learn_open = !c.learn_open;
        c.picking = false;
        return true;
    }
    if c.learn_open {
        let Some(n) = key.strip_prefix("key.keyboard.").and_then(|k| k.parse::<u8>().ok()) else {
            return false;
        };
        if (1..=4).contains(&n) {
            let id = c.learn.as_ref().map_or_else(String::new, |l| l.0.clone());
            let choice = if n == 4 { "skip".to_string() } else { n.to_string() };
            c.learn_open = false;
            drop(c);
            let _ = host.net_send_to_server(CHANNEL, format!("learnpick\t{id}\t{choice}").as_bytes());
        }
        return true;
    }
    if c.evo.is_empty() {
        return false;
    }
    // F still does what F does (this only times the hold).
    if key == "key.keyboard.f" {
        hold_f(&mut c);
        return false;
    }
    if key == "key.keyboard.v" {
        if c.evo.len() == 1 {
            drop(c);
            let _ = host.net_send_to_server(CHANNEL, b"evolve");
        } else {
            c.picking = !c.picking;
        }
        return true;
    }
    if !c.picking {
        return false;
    }
    // Numbers are the list's while it's open: one past the end mustn't cast a move.
    let Some(i) = picked(key, c.evo.len()) else {
        return key.strip_prefix("key.keyboard.").and_then(|k| k.parse::<u8>().ok()).is_some();
    };
    let to = c.evo[i].clone();
    c.picking = false;
    drop(c);
    let _ = host.net_send_to_server(CHANNEL, format!("evolve\t{to}").as_bytes());
    true
}

/// The XP multiplier and the server's median level (0 = not known).
pub fn boost() -> (f32, u32) {
    let c = PROG_CLIENT.lock().unwrap();
    (c.mult, c.median)
}

/// Hold F this long at the evolve prompt to start (a tap stays a normal F).
const HOLD_F_SECS: f32 = 0.3;

/// F is down: start timing it if there's something to evolve into.
fn hold_f(c: &mut ProgClient) {
    if !c.evo.is_empty() {
        c.f_held = Some((Instant::now(), false));
    }
}

/// Called from the HUD every frame: a held F starts the channel, or opens the list of forms.
fn hold_f_tick(host: &mut dyn Host) {
    let mut c = PROG_CLIENT.lock().unwrap();
    let Some((at, false)) = c.f_held else { return };
    if at.elapsed().as_secs_f32() < HOLD_F_SECS || c.evo.is_empty() {
        return;
    }
    c.f_held = Some((at, true));
    if c.evo.len() == 1 {
        drop(c);
        let _ = host.net_send_to_server(CHANNEL, b"evolve");
    } else {
        c.picking = !c.picking;
    }
}

/// A key came up: letting go of F during the channel cancels it.
pub fn key_up(host: &mut dyn Host, key: &str) {
    if key != "key.keyboard.f" {
        return;
    }
    let mut c = PROG_CLIENT.lock().unwrap();
    let started = matches!(c.f_held, Some((_, true)));
    let single = c.evo.len() == 1;
    c.f_held = None;
    drop(c);
    if started && single {
        let _ = host.net_send_to_server(CHANNEL, b"evolve\t-");
    }
}

/// Whether nothing evolves from this species into a playable form: it can have an Ultimate.
pub fn is_final(species: &str) -> bool {
    EVOS.get(species).is_none_or(|e| e.evo.iter().all(|(to, _)| kit(to).is_none()))
}

/// The playable species this one can evolve into.
pub fn evolutions(species: &str) -> Vec<String> {
    EVOS.get(species).map_or(Vec::new(), |e| e.evo.iter().filter(|(to, _)| kit(to).is_some()).map(|(to, _)| to.clone()).collect())
}

/// Every level-up move of this species.
pub fn learnset(species: &str) -> Vec<String> {
    EVOS.get(species).map_or(Vec::new(), |e| e.learn.iter().map(|(_, id)| id.clone()).collect())
}

/// The account level this client was last told.
pub fn client_level() -> u32 {
    PROG_CLIENT.lock().unwrap().xp.map_or(1, |x| x.0)
}

/// Which option a number key picks, if it's in range.
fn picked(key: &str, options: usize) -> Option<usize> {
    let n: usize = key.strip_prefix("key.keyboard.")?.parse().ok()?;
    (1..=options).contains(&n).then(|| n - 1)
}

/// The open list's text, four forms to a row so all eight of Eevee's fit.
fn pick_rows(names: &[String]) -> Vec<String> {
    let numbered: Vec<String> = names.iter().enumerate().map(|(i, n)| format!("{} {n}", i + 1)).collect();
    numbered.chunks(4).map(|r| r.join("    ")).collect()
}

/// The XP bar above the move bar, and the evolution prompt.
pub fn hud(host: &mut dyn Host, b: &mut HudBatch) {
    hold_f_tick(host);
    let c = PROG_CLIENT.lock().unwrap();
    let Some((lvl, xp, need)) = c.xp else { return };
    if !is_pokemon() {
        return;
    }
    let (w, h) = (182.0f32, 3.0f32);
    let (x, y) = ((b.width - w) / 2.0, b.height - 46.0);
    rect(host, b, x - 1.0, y - 1.0, w + 2.0, h + 2.0, 0xC010_1418);
    let frac = (xp as f32 / need.max(1) as f32).clamp(0.0, 1.0);
    rect(host, b, x, y, w * frac, h, 0xFF7C_E85A);
    let label = format!("Lv {lvl}");
    let lw = host.text_width(&label);
    host.text(b, &label, [x - lw - 4.0, y - 3.0], 0x9CF27A, true);
    if let Some((g, why, at)) = &c.gain {
        if at.elapsed().as_secs_f32() < 2.5 {
            let t = if why == "-" { format!("+{g} XP") } else { format!("+{g} XP  {why}") };
            host.text(b, &t, [x + w + 4.0, y - 3.0], 0x9CF27A, true);
        }
    }
    let names: Vec<String> = c.evo.iter().map(|to| kit(to).map_or(to.clone(), |k| k.name.clone())).collect();
    let mut lines: Vec<String> = match names.as_slice() {
        [] => Vec::new(),
        [name] => vec![format!("Ready to evolve into {name}!  Hold F (stay still)")],
        _ if c.picking => {
            let mut l = vec!["Press a number to evolve (stay still), or hold F to cancel".to_string()];
            l.extend(pick_rows(&names));
            l
        }
        _ => vec![format!("Ready to evolve: {} forms to choose from!  Hold F", names.len())],
    };
    if let Some((_, name, [q, e, r])) = &c.learn {
        if c.learn_open {
            lines.insert(0, format!("1 {q} (Q)    2 {e} (E)    3 {r} (R)    4 Skip"));
            lines.insert(0, format!("Teach {name}: pick a move to replace, or [N] to close"));
        } else {
            lines.insert(0, format!("New move: {name}!  [N] Learn it"));
        }
    }
    lines.reverse();
    for (i, t) in lines.iter().enumerate() {
        // The open list sits above the chat lines that appear down here.
        let lift = if c.picking || c.learn_open { 38.0 } else { 0.0 };
        let (cx, ly) = (b.width / 2.0, y - 14.0 - lift - 11.0 * i as f32);
        super::guide::keyed_centred(host, b, t, cx, ly, 0xFFE9A0);
    }
}

