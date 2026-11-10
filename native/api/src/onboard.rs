//! Onboarding (design doc "Onboarding: Nests, Tutorial Meadow & Hatchery"): egg-hatch spawns at
//! Nests, the six-station Tutorial Meadow on a loaner Eevee, and the Hatchery starter pick.
//!
//! Everything is driven by sidecar files the map generator writes into the world folder:
//! `nests.json`, `tutorial.json`, `hatchery.json` and `map.json` (zones, lab.json style). A world
//! without them behaves exactly as before: no hatch, no tutorial, no bounds.
//!
//! Saved per player in `<world>/capipimon-players.json` (doc fields only): `tutorial_done`,
//! `starter_species`, `unlocked_lines`, `last_nest`. When that file is first made, every player
//! the world already knows (`players/data`) is counted as done, so nobody on a live server is
//! dropped into the Meadow by an update.
//!
//! Wire (channel `capipimon:play`):
//! - server → client: `hatch\t<name>\t<species>\t<secs>` / `hatch\t<name>\t-` (everyone: draw an
//!   egg in place of that player), `hatchme\t<secs>` / `hatchme\t-` (owner: skip prompt),
//!   `card\t<species>` / `card\t-`, `confirm\t<species>`, `struggle\t1` / `struggle\t0`.
//! - client → server: `skip`, `use` (F), `struggle\tok`.

#[cfg(feature = "capipimon_server")]
use crate::play::prof;
use super::*;
use serde::{Deserialize, Serialize};

// The server half: private, never published (docs/CLIENT_SERVER_SPLIT.md).
#[cfg(feature = "capipimon_server")]
include!("server/onboard.rs");

const SKIP_AFTER_TICKS: u64 = 10;

const CONFIRM_TICKS: u64 = 100;

/// The Nest PC: switch species, relearn moves (GDD 2.3, 3.8).
#[path = "pc.rs"]
pub mod pc;

/// Levels, XP, evolution, research (GDD 4, 3.8, 2.3).
#[path = "progress.rs"]
pub mod progress;
#[cfg(feature = "capipimon_server")]
#[path = "server/rewards.rs"]
pub mod rewards;

/// Wild NPC Pokémon around the Nests (GDD 5.1).
#[cfg(feature = "capipimon_server")]
#[path = "server/wilds.rs"]
pub mod wilds;
#[cfg(feature = "capipimon_server")]
#[path = "server/npc.rs"]
pub mod npc;
#[cfg(feature = "capipimon_server")]
#[path = "server/heal.rs"]
pub mod heal;
/// Berry Grove capture (GDD 4.2).
#[cfg(feature = "capipimon_server")]
#[path = "server/groves.rs"]
pub mod groves;
#[path = "guide.rs"]
pub mod guide;
pub mod weather;
#[cfg(feature = "capipimon_server")]
#[path = "server/hordes.rs"]
pub mod hordes;
pub mod worldmap;
#[cfg(feature = "capipimon_server")]
#[path = "server/dungeons.rs"]
pub mod dungeons;

/// Catching, trainers and parties (GDD 6).
#[path = "catch.rs"]
pub mod catch;

/// Event Night run sheet, legendaries, NPC Pippa announcements (GDD 1.4, 8).
#[path = "event.rs"]
pub mod event;
/// Legendary outcomes per season: escape title and cosmetic, escape-rate knobs (GDD 8.3).
#[cfg(feature = "capipimon_server")]
#[path = "server/legends.rs"]
pub mod legends;
/// The Tastiest Power Cord, Hunt 3's relic (GDD 6.6).
#[cfg(feature = "capipimon_server")]
#[path = "server/relic.rs"]
pub mod relic;

/// Live slots and death rotation on Event Nights (GDD 2.4).
#[cfg(feature = "capipimon_server")]
#[path = "server/rotation.rs"]
pub mod rotation;

/// Optional PvE features (GDD 5.2, 5.3, 5.4, 4.7), all off by default.
#[cfg(feature = "capipimon_server")]
#[path = "server/pve.rs"]
pub mod pve;

/// Daily/weekly quests, first KO of the day (GDD 5.5).
#[cfg(feature = "capipimon_server")]
#[path = "server/quests.rs"]
pub mod quests;
/// Duels and spars (roadmap, Season 1).
#[cfg(feature = "capipimon_server")]
#[path = "server/duels.rs"]
pub mod duels;
/// Hats, auras and trails.
#[path = "cosmetics.rs"]
pub mod cosmetics;
/// Titles, the welcome-back card, the community goal bar and the ignore list (client half).
#[path = "launch_ui.rs"]
pub mod launch_ui;
/// The Capipidex: the entry list, the `/dex` book, and (server) marks and rewards.
#[path = "dex.rs"]
pub mod dex;
/// Evolution stones and the Move Tutor: the Tutor's menu, and (server) drops, stones and lessons.
#[path = "tutor.rs"]
pub mod tutor;
/// Photo mode (`/photo`, client only).
#[path = "photo.rs"]
pub mod photo;
/// Titles, the welcome-back card, the community goal and `/ignore` (server half).
#[cfg(feature = "capipimon_server")]
#[path = "server/launch.rs"]
pub mod launch;
/// Crews: small player groups with a tag, a banner, a chat, a dex, weekly goals and a season board.
#[path = "crew.rs"]
pub mod crew;
use std::path::PathBuf;
use std::sync::Arc;

/// The Hatchery's starter list (`tools/starters.py` → `data/starters.json`).
const STARTERS: &str = include_str!("../data/starters.json");

#[derive(Clone, Debug, Deserialize)]
pub struct Starter {
    pub species: String,
    pub pool: String,
    pub role: String,
    pub featured: bool,
}

pub fn starters() -> &'static [Starter] {
    static LIST: LazyLock<Vec<Starter>> = LazyLock::new(|| serde_json::from_str(STARTERS).unwrap_or_default());
    &LIST
}

fn starter(species: &str) -> Option<&'static Starter> {
    starters().iter().find(|s| s.species == species)
}

// ---------------------------------------------------------------- client

#[derive(Default)]
pub struct ObClient {
    /// Player name → (species, started, seconds): drawn as an egg.
    hatching: HashMap<String, (String, Instant, f32)>,
    /// This client's own hatch: when the skip prompt shows.
    mine: Option<Instant>,
    card: Option<String>,
    confirm: Option<(String, Instant)>,
    struggle: Option<Instant>,
    /// A missed Space press, for a moment.
    missed: Option<Instant>,
}

pub static OB_CLIENT: LazyLock<Mutex<ObClient>> = LazyLock::new(Default::default);

/// A message for the onboarding client; true when it was one.
pub fn heard(f: &[&str]) -> bool {
    if crew::heard(f) || cosmetics::draw::heard(f) || catch::heard(f) || event::heard(f) || pc::heard(f) || progress::heard(f) || weather::heard(f) || worldmap::heard(f) || guide::heard(f) || launch_ui::heard(f) || dex::heard(f) || tutor::heard(f) {
        return true;
    }
    let mut c = OB_CLIENT.lock().unwrap();
    match f {
        ["hatch", name, "-", ..] => {
            c.hatching.remove(*name);
        }
        ["hatch", name, species, secs, ..] => {
            let secs = secs.parse::<f32>().unwrap_or(1.5).clamp(0.1, 10.0);
            c.hatching.insert(name.to_string(), (species.to_string(), Instant::now(), secs));
        }
        ["hatchme", "-", ..] => c.mine = None,
        ["hatchme", ..] => c.mine = Some(Instant::now()),
        ["card", "-", ..] => {
            c.card = None;
            c.confirm = None;
        }
        ["card", species, ..] => {
            c.card = Some(species.to_string());
            c.confirm = None;
        }
        ["confirm", species, ..] => c.confirm = Some((species.to_string(), Instant::now())),
        ["struggle", on, ..] => c.struggle = (*on == "1").then(Instant::now),
        _ => return false,
    }
    true
}

pub fn forget() {
    pc::forget();
    tutor::forget();
    cosmetics::draw::forget();
    progress::forget();
    weather::forget();
    worldmap::forget();
    guide::forget();
    catch::forget();
    event::forget();
    launch_ui::forget();
    dex::forget();
    photo::forget();
    *OB_CLIENT.lock().unwrap() = ObClient::default();
}

/// If `name` is hatching: its species and how far along (0..1).
pub fn hatching_of(name: &str) -> Option<(String, f32)> {
    let c = OB_CLIENT.lock().unwrap();
    let (s, at, secs) = c.hatching.get(name)?;
    let t = at.elapsed().as_secs_f32() / secs;
    (t < 1.2).then(|| (s.clone(), t.min(1.0)))
}

/// A tutorial `struggle ok` sooner than this after the bar appears is ignored: the needle needs
/// 4.8 ticks to reach the green zone.
const STRUGGLE_MIN_TICKS: u64 = 4;

/// The struggle needle: 0..1, back and forth every 1.6 s; the green zone is 0.3..0.7 (40%).
fn needle(at: Instant) -> f32 {
    let t = (at.elapsed().as_secs_f32() / 0.8) % 2.0;
    if t < 1.0 { t } else { 2.0 - t }
}
const ZONE: (f32, f32) = (0.3, 0.7);

/// Keys for onboarding; true when taken. Move keys skip this client's own hatch; F is "use";
/// Space is the struggle.
pub fn key(host: &mut dyn Host, key: &str) -> bool {
    if photo::key(key) || dex::key(key) || launch_ui::key(key) {
        return true;
    }
    if crate::emotes::key(host, key, pc::is_open() || cosmetics::draw::is_open() || tutor::is_open()) || catch::key(host, key) || pc::key(host, key) || tutor::key(host, key) || cosmetics::draw::key(host, key) || worldmap::key(host, key) || guide::key(host, key) || progress::key(host, key) {
        return true;
    }
    let mut c = OB_CLIENT.lock().unwrap();
    if c.mine.is_some() && matches!(key, "key.keyboard.1" | "key.keyboard.2" | "key.keyboard.3" | "key.keyboard.4" | "key.keyboard.q" | "key.keyboard.e" | "key.keyboard.r") {
        drop(c);
        let _ = host.net_send_to_server(CHANNEL, b"skip");
        return true;
    }
    match key {
        "key.keyboard.f" => {
            drop(c);
            let msg = match super::cam_now() {
                Some(p) => format!("use\t{:.2}\t{:.2}\t{:.2}", p[0], p[1], p[2]),
                None => "use".to_string(),
            };
            let _ = host.net_send_to_server(CHANNEL, msg.as_bytes());
            true
        }
        "key.keyboard.space" => {
            let Some(at) = c.struggle else { return false };
            let n = needle(at);
            if (ZONE.0..=ZONE.1).contains(&n) {
                drop(c);
                let _ = host.net_send_to_server(CHANNEL, b"struggle\tok");
            } else {
                c.missed = Some(Instant::now());
            }
            false
        }
        _ => false,
    }
}

/// Left click while hatching skips too (it's the basic attack key).
pub fn click_skips(host: &mut dyn Host) -> bool {
    if OB_CLIENT.lock().unwrap().mine.is_some() {
        let _ = host.net_send_to_server(CHANNEL, b"skip");
        return true;
    }
    false
}

pub fn hud(host: &mut dyn Host, b: &mut HudBatch) {
    weather::hud(host, b);
    progress::hud(host, b);
    pc::hud(host, b);
    tutor::hud(host, b);
    cosmetics::draw::hud(host, b);
    catch::hud(host, b);
    event::hud(host, b);
    launch_ui::hud(host, b);
    dex::hud(host, b);
    crate::emotes::hud(host, b);
    let c = OB_CLIENT.lock().unwrap();
    let (cx, cy) = (b.width / 2.0, b.height / 2.0);
    if let Some(at) = c.mine {
        let t = "Hatching...";
        let w = host.text_width(t);
        host.text(b, t, [cx - w / 2.0, cy + 20.0], 0xFFFFFF, true);
        if at.elapsed().as_secs_f32() >= SKIP_AFTER_TICKS as f32 / 20.0 {
            let t = "Press a move key to skip";
            let w = host.text_width(t);
            host.text(b, t, [cx - w / 2.0, cy + 31.0], 0xC0C8D8, true);
        }
    }
    if let Some(at) = c.struggle {
        let (w, h) = (160.0f32, 10.0f32);
        let (x, y) = (cx - w / 2.0, cy - 32.0);
        pill(host, b, x - 2.0, y - 2.0, w + 4.0, h + 4.0, HUD_PANEL);
        rect(host, b, x, y, w, h, 0xFF40_2020);
        rect(host, b, x + w * ZONE.0, y, w * (ZONE.1 - ZONE.0), h, 0xFF2F_9E5A);
        let nx = x + w * needle(at);
        rect(host, b, nx - 1.0, y - 3.0, 3.0, h + 6.0, 0xFFFF_FFFF);
        let missed = c.missed.is_some_and(|m| m.elapsed().as_secs_f32() < 0.6);
        let t = if missed { "Missed! Try again" } else { "Struggle! Press Space in the green" };
        let tw = host.text_width(t);
        host.text(b, t, [cx - tw / 2.0, y - 13.0], if missed { 0xFF6060 } else { 0xFFFFFF }, true);
    }
    let Some(species) = c.card.clone() else { return };
    let confirm = c.confirm.as_ref().filter(|(s, at)| *s == species && at.elapsed().as_secs_f32() < CONFIRM_TICKS as f32 / 20.0).is_some();
    drop(c);
    let Some(k) = kit(&species) else { return };
    let st = starter(&species);
    let (w, h) = (170.0f32, 112.0f32);
    let (x, y) = (b.width - w - 8.0, cy - h / 2.0);
    pill(host, b, x, y, w, h, HUD_PANEL);
    pill(host, b, x, y, w, 13.0, 0xFFB8_2A1A);
    let star = if st.is_some_and(|s| s.featured) { " \u{2605}" } else { "" };
    host.text(b, &format!("{}{star}", k.name), [x + 4.0, y + 3.0], 0xFFFFFF, true);
    if let Some(s) = st {
        let role = s.role.to_uppercase();
        let rw = host.text_width(&role);
        host.text(b, &role, [x + w - 4.0 - rw, y + 3.0], 0xFFE9A0, true);
    }
    let mut tx = x + 4.0;
    for t in &k.types {
        let label = t.to_uppercase();
        let tw = host.text_width(&label);
        pill(host, b, tx, y + 16.0, tw + 6.0, 10.0, 0xFF00_0000 | type_color(t));
        host.text(b, &label, [tx + 3.0, y + 17.0], 0xFFFFFF, true);
        tx += tw + 9.0;
    }
    // HP, Atk (the better attack), Spe against 150.
    let bars = [("HP", k.base[0], 0xFF4C_D964u32), ("Atk", k.base[1].max(k.base[3]), 0xFFFF_8A3D), ("Spe", k.base[5], 0xFF3F_A9F5)];
    for (i, (label, v, col)) in bars.iter().enumerate() {
        let by = y + 30.0 + i as f32 * 10.0;
        host.text(b, label, [x + 4.0, by], 0xC0C8D8, true);
        pill(host, b, x + 28.0, by + 1.0, w - 34.0, 6.0, 0xFF26_2626);
        rect(host, b, x + 29.0, by + 2.0, (w - 36.0) * (v / 150.0).clamp(0.0, 1.0), 4.0, *col);
    }
    for (i, m) in k.moves.iter().take(4).enumerate() {
        let my = y + 62.0 + i as f32 * 9.0;
        rect(host, b, x + 4.0, my + 2.0, 4.0, 4.0, 0xFF00_0000 | type_color(&m.kind));
        host.text(b, &fit(host, &m.name, w - 16.0), [x + 11.0, my], 0xFFFFFF, true);
    }
    let prompt = if confirm { format!("[F] again: hatch {}?", k.name) } else { "[F] Choose".to_string() };
    guide::keyed_centred(host, b, &prompt, x + w / 2.0, y + h - 11.0, if confirm { 0xFFE9A0 } else { 0xFFFFFF });
    if confirm {
        let note = "Research unlocks the other lines later.";
        let nw = host.text_width(note);
        host.text(b, note, [cx - nw / 2.0, y + h + 4.0], 0xC0C8D8, true);
    }
}

/// A light tint of the species' first type, for its egg.
pub fn egg_tint(species: &str) -> u32 {
    let base = kit(species).and_then(|k| k.types.first()).map_or(0xE0E0E0, |t| type_color(t));
    let mix = |sh: u32| (((base >> sh) & 0xFF) + 0xFF * 2) / 3;
    (mix(16) << 16) | (mix(8) << 8) | mix(0)
}

pub fn featured(species: &str) -> bool {
    starter(species).is_some_and(|s| s.featured)
}

