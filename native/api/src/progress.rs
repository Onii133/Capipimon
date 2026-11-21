//! Persistent progression (GDD 4.1 to 4.5, 3.8): each Pokémon's level and XP, evolution, daily bonus.
//!
//! Saved per player in `capipimon-players.json` (`levels`: each owned line's level and XP into it,
//! `last_day`, `streak`, `research`; see docs/LEVELS.md). XP goes to the player's own Pokémon, and
//! a Pokémon plays at its line's saved level (`/capi become <species> 50` sets that line's).
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
    /// Evolutions that only happen at a place: (form, level, gym id). Regional forms (tools/forms.py).
    #[serde(default)]
    place: Vec<(String, u32, String)>,
}

static EVOS: LazyLock<HashMap<String, Evo>> =
    LazyLock::new(|| serde_json::from_str(include_str!("../data/evolutions.json")).unwrap_or_default());

/// What the Nest PC offers: the species' signature move first (GDD 3.1: it is in the move pool,
/// not on a key of its own), its own kit moves (the LMB basic attack goes on LMB only), then its
/// learnset so far.
pub fn teachable(species: &str, level: u32) -> Vec<Move> {
    let mut out: Vec<Move> = sig::signature(species).filter(|_| !sig::plain_ditto(species)).map(|s| s.mv.clone()).into_iter().collect();
    out.extend(kit(species).map_or_else(Vec::new, |k| k.moves.iter().take(4).cloned().collect()));
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
    evo: Vec<String>,
    /// The XP multiplier and the server's median level, for the HUD (GDD 11.4).
    mult: f32,
    median: u32,
    /// F went down at the evolve prompt: when, and whether the hold has fired.
    f_held: Option<(Instant, bool)>,
    /// A move on offer: id, name, and the Q/E/R moves it could replace.
    learn: Option<(String, String, [String; 3])>,
}

pub static PROG_CLIENT: LazyLock<Mutex<ProgClient>> = LazyLock::new(Default::default);

pub fn heard(f: &[&str]) -> bool {
    let mut c = PROG_CLIENT.lock().unwrap();
    match f {
        ["xp", lvl, xp, need, _gain, _why, more @ ..] => {
            c.mult = more.get(1).and_then(|r| r.parse().ok()).unwrap_or(1.0);
            c.median = more.get(2).and_then(|r| r.parse().ok()).unwrap_or(0);
            c.xp = Some((lvl.parse().unwrap_or(1), xp.parse().unwrap_or(0), need.parse().unwrap_or(60)));
        }
        ["learnoffer", "-", ..] => {
            c.learn = None;
        }
        ["learnoffer", id, name, q, e, r, ..] => {
            c.learn = Some((id.to_string(), name.to_string(), [q.to_string(), e.to_string(), r.to_string()]));
        }
        ["evo", "-", ..] => {
            c.evo.clear();
        }
        ["evo", to, ..] => {
            c.evo = to.split(',').map(str::to_string).collect();
        }
        _ => return false,
    }
    true
}

pub fn forget() {
    *PROG_CLIENT.lock().unwrap() = ProgClient::default();
}

/// Only F's hold timing is here: picking a move or a form happens in the Tab menu (GDD 3.1 keybinds).
pub fn key(_host: &mut dyn Host, key: &str) -> bool {
    let mut c = PROG_CLIENT.lock().unwrap();
    // F still does what F does (this only times the hold).
    if key == "key.keyboard.f" {
        hold_f(&mut c);
    }
    false
}

/// What's on offer now: (a move to learn, a form to evolve into).
pub fn offers() -> (bool, bool) {
    let c = PROG_CLIENT.lock().unwrap();
    (c.learn.is_some(), !c.evo.is_empty())
}

/// Your Pokémon's level, XP and XP to the next level, once the server has said.
pub fn exp() -> Option<(u32, u64, u64)> {
    PROG_CLIENT.lock().unwrap().xp
}

/// The move on offer and the Q/E/R moves it could replace.
pub fn learn_choices() -> Option<(String, [String; 3])> {
    PROG_CLIENT.lock().unwrap().learn.as_ref().map(|l| (l.1.clone(), l.2.clone()))
}

/// Teach the offered move over slot `i` (0-2 = Q/E/R), or turn it down (3).
pub fn pick_learn(host: &mut dyn Host, i: usize) {
    let id = PROG_CLIENT.lock().unwrap().learn.as_ref().map(|l| l.0.clone());
    let Some(id) = id else { return };
    let choice = if i >= 3 { "skip".to_string() } else { (i + 1).to_string() };
    let _ = host.net_send_to_server(CHANNEL, format!("learnpick	{id}	{choice}").as_bytes());
}

/// The forms on offer, by name.
pub fn evo_names() -> Vec<String> {
    PROG_CLIENT.lock().unwrap().evo.iter().map(|to| kit(to).map_or(to.clone(), |k| k.name.clone())).collect()
}

/// Start evolving into form `i` of the offer (stay still while it happens).
pub fn pick_evolve(host: &mut dyn Host, i: usize) {
    let c = PROG_CLIENT.lock().unwrap();
    let msg = match c.evo.len() {
        0 => return,
        1 => "evolve".to_string(),
        _ => match c.evo.get(i) {
            Some(to) => format!("evolve	{to}"),
            None => return,
        },
    };
    drop(c);
    let _ = host.net_send_to_server(CHANNEL, msg.as_bytes());
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
        drop(c);
        super::hub::open_evolve();
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

/// Whether nothing evolves from this species into a playable form: it can have a Z-Move.
pub fn is_final(species: &str) -> bool {
    EVOS.get(species).is_none_or(|e| e.evo.iter().all(|(to, _)| kit(to).is_none()))
}

/// The playable species this one can evolve into.
pub fn evolutions(species: &str) -> Vec<String> {
    EVOS.get(species).map_or(Vec::new(), |e| {
        let all = e.evo.iter().map(|(to, _)| to).chain(e.place.iter().map(|(to, _, _)| to));
        all.filter(|to| kit(to).is_some()).cloned().collect()
    })
}

/// Every level-up move of this species.
pub fn learnset(species: &str) -> Vec<String> {
    EVOS.get(species).map_or(Vec::new(), |e| e.learn.iter().map(|(_, id)| id.clone()).collect())
}

/// Your Pokémon's level, as this client was last told.
pub fn client_level() -> u32 {
    PROG_CLIENT.lock().unwrap().xp.map_or(1, |x| x.0)
}

/// The evolve and new-move prompts above the move bar (the EXP bar is on the card, play.rs).
pub fn hud(host: &mut dyn Host, b: &mut HudBatch) {
    hold_f_tick(host);
    let c = PROG_CLIENT.lock().unwrap();
    if c.xp.is_none() || !is_pokemon() {
        return;
    }
    let y = b.height - 46.0;
    let names: Vec<String> = c.evo.iter().map(|to| kit(to).map_or(to.clone(), |k| k.name.clone())).collect();
    let mut lines: Vec<String> = match names.as_slice() {
        [] => Vec::new(),
        [name] => vec![format!("Ready to evolve into {name}!  Hold [F] (stay still)")],
        _ => vec![format!("Ready to evolve: {} forms to choose from!  Hold [F]", names.len())],
    };
    if let Some((_, name, _)) = &c.learn {
        lines.insert(0, format!("New move: {name}!  Learn it from the [Tab] menu"));
    }
    lines.reverse();
    for (i, t) in lines.iter().enumerate() {
        let (cx, ly) = (b.width / 2.0, y - 14.0 - 11.0 * i as f32);
        super::guide::keyed_centred(host, b, t, cx, ly, 0xFFE9A0);
    }
}

