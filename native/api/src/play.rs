//! M2 spike: play as a Pokémon.
//!
//! The player *is* the Pokémon. The server keeps each player's species, level, energy and cooldowns,
//! sets their hitbox with the vanilla `minecraft:scale` attribute and their HP with `max_health`,
//! and resolves every move. The client draws the species' model in place of the player
//! (`player_submit`), sends move keys 1-4 to the server and shows a HUD.
//!
//! Wire format (channel `capipimon:play`, UTF-8, tab-separated):
//! - client → server: `cast\t<slot>`
//! - server → client: `species\t<player name>\t<species or ->` (to everyone),
//!   `state\t<species>\t<level>\t<hp>\t<max hp>\t<energy>\t<cd1>..<cd4>` (to the owner; `state\t-`: not a Pokémon any more),
//!   `cast\t<player name>\t<anim>` (to everyone, for the cast animation),
//!   `ko\t<seconds left>\t<trainer name or ->` (to a fainted player, once a second; `ko\t0\t-` ends it).

use horizon_client::mod_api::{
    CameraDistanceEvent, CameraPoseEvent, Corner, Host, HudBatch, HudEvent, KeyEvent, LivingIncomingDamageEvent, MovementInputEvent, NativeCommand,
    NativeServerPlayer,
    NetFromClientEvent, NetFromServerEvent, PerformCommandEvent, RegisterCommandsEvent, ServerTickEvent, ServerTickPhase,
    Text,
};
use horizon_client::mod_api::{Compound, Tag};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

// The server half: private, never published (docs/CLIENT_SERVER_SPLIT.md).
#[cfg(feature = "capipimon_server")]
include!("server/play.rs");

/// Energy regained per second; the bar is 0..100.
const ENERGY_REGEN: f32 = 12.0;

/// Fake Out works this long after coming out (spec: first 3 s after deploying).
const FAKE_OUT_TICKS: u64 = 60;

/// A cone's half angle (spec cones: Swift, Hyper Voice, Blizzard, Icy Wind, Air Cutter, ...).
const CONE_HALF_DEG: f64 = 30.0;

/// What a dodge roll costs in PP. There is no cooldown: PP is the limit, so a roll competes with moves
/// and guarding for the same bar. Four in a row empty a full bar; at rest one comes back every 2 s.
pub const DODGE_COST: f32 = 25.0;
/// A new roll can't start until the last one's tumble has played out.
const DODGE_LOCK: f32 = ROLL_SECS;

/// Whether a roll may start with this much PP and this much of the last roll left to play.
fn can_dodge(energy: f32, lock: f32) -> bool {
    lock <= 0.0 && energy >= DODGE_COST
}

/// The move keys, slot by slot.
const SLOT_KEYS: [&str; 6] = ["LMB", "Q", "E", "R", "", "Z"];

fn dir(yaw: f32, pitch: f32) -> [f64; 3] {
    let (y, p) = (f64::from(yaw).to_radians(), f64::from(pitch).to_radians());
    [-y.sin() * p.cos(), -p.sin(), y.cos() * p.cos()]
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Whether `p` is inside a block's collision box.
fn solid(host: &dyn Host, p: [f64; 3]) -> bool {
    let b = [p[0].floor() as i32, p[1].floor() as i32, p[2].floor() as i32];
    let l = [p[0] - f64::from(b[0]), p[1] - f64::from(b[1]), p[2] - f64::from(b[2])];
    host.block_collision(b)
        .iter()
        .any(|x| (x[0]..=x[3]).contains(&l[0]) && (x[1]..=x[4]).contains(&l[1]) && (x[2]..=x[5]).contains(&l[2]))
}

/// Nests, the Tutorial Meadow and the Hatchery (onboarding design doc).
#[path = "onboard.rs"]
pub mod onboard;
pub use onboard::progress;
pub use onboard::cosmetics;

/// Flight: winged Pokémon take off with a double-tapped Space and glide the vanilla elytra way.
#[path = "flight.rs"]
pub mod flight;

/// Signature moves (slot C), their statuses, and Ditto's Transform.
#[path = "sig.rs"]
pub mod sig;
#[cfg(feature = "capipimon_server")]
#[path = "server/status.rs"]
pub mod status;
#[cfg(feature = "capipimon_server")]
#[path = "server/rewind.rs"]
mod rewind;
/// Balance log: one line per PvP knock-out (server/balance.rs).
#[cfg(feature = "capipimon_server")]
#[path = "server/balance.rs"]
mod balance;
#[path = "ultimate.rs"]
pub mod ultimate;
/// Support moves: buffs, debuffs, status shots, heals, drain, and support specialists.
#[path = "support.rs"]
pub mod support;

/// Chat filter, personal-info block and /report (GDD 11.1).
#[cfg(feature = "capipimon_server")]
#[path = "server/automod.rs"]
pub mod automod;
/// Emotes, the server half (the wheel and drawing are crate::emotes, client side).
#[cfg(feature = "capipimon_server")]
#[path = "server/emotes.rs"]
pub mod emotes_srv;
/// Pokémon cries (content sounds), on emotes and hatching.
#[cfg(feature = "capipimon_server")]
#[path = "server/cries.rs"]
pub mod cries;

/// Talent NPCs as player models in their own skins.
#[cfg(feature = "capipimon_server")]
#[path = "server/npc_skins.rs"]
pub mod npc_skins;

/// Opt-in tick timing (CAPI_PROFILE=1).
#[cfg(feature = "capipimon_server")]
#[path = "server/prof.rs"]
pub mod prof;

/// Stream overlays and YouTube chat commands through files (GDD 10).
#[cfg(feature = "capipimon_server")]
#[path = "server/stream.rs"]
pub mod stream;

/// Every `/capiadmin` and `/capi` usage, declared for the chat's command tree and completion.
#[cfg(feature = "capipimon_server")]
#[path = "server/commands.rs"]
pub mod commands;

pub const CHANNEL: &str = "capipimon:play";
/// The wire version shared by the public client and the private server. Bump it whenever a message changes shape;
/// the server refuses clients on another version (`proto` / `hello`, docs/CLIENT_SERVER_SPLIT.md).
pub const PROTOCOL: u32 = 2;

/// Seconds a server message gives, clamped to `0..=max`. Anything that isn't a finite number (NaN, inf, junk) is 0, so
/// building a `Duration` from it can't panic.
pub(crate) fn finite_secs(text: &str, max: f32) -> f32 {
    text.parse::<f32>().ok().filter(|s| s.is_finite()).unwrap_or(0.0).clamp(0.0, max)
}
/// How long a cast animation plays on the client.
pub const CAST_ANIM_SECS: f32 = 0.6;

// ---------------------------------------------------------------- kits
//
// Kits come from two embedded sources: `data/kits_auto.json` (every species, built by
// `recon/tools/build_kits.py`) and the hand-tuned `data/kits/*.json`, which replace the auto kit
// of their species. Both are read strictly: an unknown field, a missing one or a bad value makes
// the kit unusable, and the reason is kept for `/capi kits` (the `log` crate doesn't reach latest.log).

use serde::Deserialize;

const KIT_SCHEMA: u32 = 1;

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Auto,
    Hand,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Physical,
    Special,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Shape {
    Beam,
    Dash,
    Burst,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub kind: String,
    pub chance: f32,
    /// The chance against a Wet target instead (Shock Wave, GDD 3.9).
    #[serde(default)]
    pub wet: Option<f32>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Move {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub category: Category,
    pub power: f32,
    pub shape: Shape,
    pub range: f32,
    pub width: f32,
    pub cooldown: f32,
    pub energy: f32,
    pub windup: f32,
    pub anim: String,
    pub particle: String,
    #[serde(default)]
    pub status: Option<Status>,
    /// A generic move added because the species has too few of its own.
    #[serde(default)]
    pub filler: bool,
    /// Featured kits (GDD 3.9): the hitbox form, instead of the move data's.
    #[serde(default)]
    pub form: Option<String>,
    /// A cone's full angle in degrees (0: the default).
    #[serde(default)]
    pub cone: f32,
    /// The wind-up in seconds, instead of the formula's.
    #[serde(default)]
    pub tele: Option<f32>,
    /// 2: a second use within 1.5 s is free.
    #[serde(default)]
    pub charges: u8,
    /// [speed multiplier, seconds] on everyone it hits.
    #[serde(default)]
    pub slow: Option<[f32; 2]>,
    /// Seconds of channel after it leaves (no other move).
    #[serde(default)]
    pub busy: f32,
    /// Stuns only the grounded; the airborne take a quarter.
    #[serde(default)]
    pub ground_wave: bool,
    /// Knockback distance instead of the move's own.
    #[serde(default)]
    pub knock: Option<f32>,
    /// A support move (power 0): what it does instead of damage.
    #[serde(default)]
    pub support: Option<support::Support>,
}

impl Move {
    pub fn physical(&self) -> bool {
        self.category == Category::Physical
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
struct Base {
    hp: f32,
    atk: f32,
    def: f32,
    spa: f32,
    spd: f32,
    spe: f32,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
struct Hitbox {
    width: f32,
    height: f32,
    base_scale: f32,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
struct KitFile {
    schema: u32,
    species: String,
    name: String,
    source: Source,
    types: Vec<String>,
    base: Base,
    hitbox: Hitbox,
    moves: Vec<Move>,
    /// A featured kit's own Z-Move (Z).
    #[serde(default)]
    ultimate: Option<Move>,
    /// Legendaries (GDD 8.2): HP and blocks a second that don't depend on level.
    #[serde(default)]
    fixed_hp: Option<f32>,
    #[serde(default)]
    fixed_speed: Option<f32>,
}

#[derive(Clone, Debug)]
pub struct Kit {
    pub species: String,
    pub name: String,
    pub source: Source,
    pub types: Vec<String>,
    /// hp, atk, def, spa, spd, spe.
    pub base: [f32; 6],
    pub height: f32,
    pub base_scale: f32,
    pub moves: Vec<Move>,
    pub ultimate: Option<Move>,
    pub fixed_hp: Option<f32>,
    pub fixed_speed: Option<f32>,
    /// How tall the Pokémon stands in blocks: the model's measured height
    /// (`data/model_sizes.json`, from `tools/model_sizes.py`) when it's plausible, else the kit's
    /// hitbox height × base scale. Camera, hit tests, beam origin and name plate all use it.
    pub body: f32,
    /// Support specialists: healer, buffer, hexer or spreader (`data/support.json`).
    pub role: Option<String>,
}

/// Measured rest-pose model sizes, species → [height, width] in blocks.
const MODEL_SIZES: &str = include_str!("../data/model_sizes.json");

/// The body height for a kit: Cobblemon's placeholder hitbox (1 × 1 at scale 1) means the
/// species has no real size data, so the model decides; otherwise the model's height when
/// it's within half to double the hitbox (a sprawled or rotated rest pose can measure far off).
fn body_height(species: &str, hit_h: f32, placeholder: bool) -> f32 {
    static SIZES: std::sync::OnceLock<HashMap<String, [f32; 2]>> = std::sync::OnceLock::new();
    let sizes = SIZES.get_or_init(|| serde_json::from_str(MODEL_SIZES).unwrap_or_default());
    match sizes.get(species).map(|s| s[0]) {
        Some(m) if placeholder => m.clamp(0.3, 9.0),
        Some(m) if m >= hit_h * 0.5 && m <= hit_h * 2.0 => m,
        _ => hit_h,
    }
}

#[cfg(not(feature = "capipimon_server"))]
const HAND_KITS: &[&str] = &[
    include_str!("../data/kits/pikachu.json"),
    // Written by tools/featured_kits.py (GDD 3.9, 8.2).
    include_str!("../data/kits/pichu.json"),
    include_str!("../data/kits/raichu.json"),
    include_str!("../data/kits/mudkip.json"),
    include_str!("../data/kits/marshtomp.json"),
    include_str!("../data/kits/swampert.json"),
    include_str!("../data/kits/squirtle.json"),
    include_str!("../data/kits/wartortle.json"),
    include_str!("../data/kits/blastoise.json"),
    include_str!("../data/kits/zapdos.json"),
    include_str!("../data/kits/lugia.json"),
    // Written by recon/tools/hand_kits.py.
    include_str!("../data/kits/wobbuffet.json"),
    include_str!("../data/kits/smeargle.json"),
    include_str!("../data/kits/ditto.json"),
    include_str!("../data/kits/magikarp.json"),
    include_str!("../data/kits/typhlosionhisui.json"),
];
#[cfg(not(feature = "capipimon_server"))]
const AUTO_KITS: &str = include_str!("../data/kits_auto.json");

fn finish(f: KitFile) -> Result<Kit, String> {
    let bad = |why: String| Err(format!("{}: {why}", f.species));
    if f.schema != KIT_SCHEMA {
        return bad(format!("schema {} (this mod reads {KIT_SCHEMA})", f.schema));
    }
    if f.types.is_empty() || f.types.len() > 2 {
        return bad(format!("{} types", f.types.len()));
    }
    if f.moves.is_empty() || f.moves.len() > 4 {
        return bad(format!("{} moves (1 to 4)", f.moves.len()));
    }
    for m in &f.moves {
        let ok = (m.power > 0.0 || (m.power == 0.0 && m.support.is_some()))
            && m.cooldown > 0.0
            && (0.0..=100.0).contains(&m.energy)
            && m.range > 0.0
            && m.width > 0.0
            && m.windup >= 0.0
            && m.status.as_ref().is_none_or(|s| (0.0..=1.0).contains(&s.chance));
        if !ok {
            return bad(format!("move {} has an out-of-range number", m.id));
        }
    }
    let b = &f.base;
    let base = [b.hp, b.atk, b.def, b.spa, b.spd, b.spe];
    if base.iter().any(|v| *v <= 0.0) || f.hitbox.width <= 0.0 || f.hitbox.height <= 0.0 || f.hitbox.base_scale <= 0.0 {
        return bad("a base stat or hitbox value isn't positive".into());
    }
    let placeholder = f.hitbox.height == 1.0 && f.hitbox.width == 1.0 && f.hitbox.base_scale == 1.0;
    let body = body_height(&f.species, f.hitbox.height * f.hitbox.base_scale, placeholder);
    Ok(Kit {
        body,
        species: f.species,
        name: f.name,
        source: f.source,
        types: f.types,
        base,
        height: f.hitbox.height,
        base_scale: f.hitbox.base_scale,
        moves: f.moves,
        ultimate: f.ultimate,
        fixed_hp: f.fixed_hp,
        fixed_speed: f.fixed_speed,
        role: None,
    })
}

struct Kits {
    by_species: HashMap<String, Kit>,
    /// Why kits were refused, for `/capi kits`.
    errors: Vec<String>,
}

fn load_kits() -> Kits {
    let mut by_species = HashMap::new();
    let mut errors = Vec::new();
    match serde_json::from_str::<HashMap<String, KitFile>>(AUTO_KITS) {
        Ok(all) => {
            for (key, f) in all {
                match finish(f) {
                    Ok(k) if k.species == key && k.source == Source::Auto => {
                        by_species.insert(key, k);
                    }
                    Ok(k) => errors.push(format!("{key}: listed as auto kit '{}' ({:?})", k.species, k.source)),
                    Err(e) => errors.push(e),
                }
            }
        }
        Err(e) => errors.push(format!("kits_auto.json: {e}")),
    }
    for (i, text) in HAND_KITS.iter().enumerate() {
        match serde_json::from_str::<KitFile>(text).map_err(|e| format!("hand kit {i}: {e}")).and_then(finish) {
            Ok(k) if k.source == Source::Hand => {
                by_species.insert(k.species.clone(), k);
            }
            Ok(k) => errors.push(format!("{}: a file in data/kits/ must say \"source\": \"hand\"", k.species)),
            Err(e) => errors.push(e),
        }
    }
    // Support specialists: their best support move in its slot (auto kits only).
    for (species, r) in support::roles() {
        let Some(k) = by_species.get_mut(species) else { continue };
        let Some(mv) = support::by_id(&r.mv) else {
            errors.push(format!("{species}: support move {} isn't in support.json", r.mv));
            continue;
        };
        if k.source == Source::Auto && (1..k.moves.len()).contains(&r.slot) && !k.moves.iter().any(|m| m.id == mv.id) {
            k.moves[r.slot] = mv.clone();
            k.role = Some(r.role.clone());
        }
    }
    errors.sort();
    Kits { by_species, errors }
}

fn all_kits() -> &'static Kits {
    static ALL: LazyLock<Kits> = LazyLock::new(load_kits);
    &ALL
}

pub fn kit(species: &str) -> Option<&'static Kit> {
    all_kits().by_species.get(species)
}

/// The ten behaviour moves from the move effects spec that no kit uses, for `/capi learn`
/// (`tools/learn_moves.py` -> `data/learn_moves.json`, in the kit move format).
#[cfg(not(feature = "capipimon_server"))]
const LEARN_MOVES: &str = include_str!("../data/learn_moves.json");

/// Every move `/capi learn` can teach, by id: each move some kit has (with the numbers of the first
/// kit, species in alphabetical order, that has it in a skill slot, else its basic slot) and the ten
/// behaviour moves.
fn learnable() -> &'static HashMap<String, Move> {
    static ALL: LazyLock<HashMap<String, Move>> = LazyLock::new(|| {
        let mut out: HashMap<String, Move> = HashMap::new();
        let kits = &all_kits().by_species;
        let mut names: Vec<&String> = kits.keys().collect();
        names.sort();
        for skill in [true, false] {
            for s in &names {
                for (i, m) in kits[*s].moves.iter().enumerate() {
                    if (i > 0) == skill {
                        out.entry(m.id.clone()).or_insert_with(|| m.clone());
                    }
                }
            }
        }
        for m in behaviour_moves() {
            out.insert(m.id.clone(), m.clone());
        }
        for m in support::moves() {
            out.entry(m.id.clone()).or_insert_with(|| m.clone());
        }
        out
    });
    &ALL
}

/// The behaviour moves (none if the embedded file doesn't read: `/capi learn` says so).
pub fn behaviour_moves() -> &'static [Move] {
    static LIST: LazyLock<Vec<Move>> = LazyLock::new(|| {
        serde_json::from_str::<Vec<Move>>(LEARN_MOVES)
            .unwrap_or_default()
            .into_iter()
            .filter(|m| m.power >= 0.0 && m.cooldown > 0.0 && m.range > 0.0 && m.width > 0.0 && (0.0..=100.0).contains(&m.energy))
            .collect()
    });
    &LIST
}

/// A move `/capi learn` can teach, by id or name ("Petal Dance", "petal-dance", "petaldance").
pub fn learn_move(name: &str) -> Option<&'static Move> {
    // A species' signature move (`sig:<species>`) sits in its move pool like any other move.
    if let Some(s) = name.strip_prefix("sig:").and_then(sig::signature) {
        return Some(&s.mv);
    }
    let id: String = name.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase();
    learnable().get(&id)
}

/// A dash move's movement (spec "Movement tech"): distance for a 1-block Pokémon, vertical
/// reach, and how it aims ("3d" pitch clamped, "ground" plane only, "full" pitch).
#[derive(Deserialize, Clone, Debug)]
pub struct DashFx {
    pub dist: f32,
    pub up: f32,
    pub aim: String,
}

/// Per-move data from the move effects spec (`tools/move_fx.py` -> `data/move_fx.json`).
#[derive(Deserialize, Clone, Debug)]
pub struct MoveFx {
    /// The spec's archetype (Melee, Lunge, Projectile, Beam, Cone, AoE, Ground Target, ...).
    pub arch: String,
    /// Wind-up before the Speed scaler and priority, seconds.
    pub base: f32,
    /// Showdown priority.
    pub prio: i32,
    /// Chance (0-1) a hit interrupts a wind-up (GDD 3.6: flinch is Interrupted).
    #[serde(default)]
    pub flinch: f32,
    /// "cone" or "ground" where the spec changes the kit's shape.
    #[serde(default)]
    pub shape: Option<String>,
    #[serde(default)]
    pub dash: Option<DashFx>,
    /// The spec has a hand entry for it.
    #[serde(default)]
    pub spec: bool,
}

pub fn move_fx(id: &str) -> Option<&'static MoveFx> {
    static TABLE: LazyLock<HashMap<String, MoveFx>> =
        LazyLock::new(|| serde_json::from_str(include_str!("../data/move_fx.json")).unwrap_or_default());
    TABLE.get(id)
}

/// Spec "Charge-up timing" (approved 2026-11-07, GDD 3.7 Telegraph row): a move's wind-up from
/// its `base` (archetype floor, accuracy, power, charge turn), Showdown `prio` and the caster's
/// base Speed `spe`. Priority +1 is a quarter (at least one 0.05 s anticipation frame), +2 or
/// more has none, below 0 adds 0.3 s a level (at most 0.6 s).
pub fn windup_secs(base: f32, prio: i32, spe: f32) -> f32 {
    let scaler = (1.2 - spe / 250.0).clamp(0.7, 1.15);
    match prio {
        p if p >= 2 => 0.0,
        1 => (base * scaler * 0.25).max(0.05),
        p if p < 0 => (base + (0.3 * (-p) as f32).min(0.6)) * scaler,
        _ => base * scaler,
    }
}

pub const MINIMIZE_DRAW: f32 = 0.6;

/// A beam leaves the Pokémon this far up its model (as a fraction of its height): the chest, about
/// where the client draws it from.
pub const BEAM_FROM: f64 = 0.6;

// ---------------------------------------------------------------- client

#[derive(Clone)]
pub struct Me {
    /// A two-charge move's first use: its slot and when.
    pub second: Option<(usize, Instant)>,
    /// A dodge roll this client predicted (double tap) that the server may not have confirmed yet.
    pub rolled: Option<Instant>,
    pub species: String,
    pub level: u32,
    pub hp: f32,
    pub max_hp: f32,
    pub energy: f32,
    pub cooldown: [f32; 6],
    /// When the server's numbers above were true; the HUD counts down from here between updates.
    pub at: Instant,
    /// Casts this client predicted (key press) that the server may not have confirmed yet.
    pub predicted: [Option<Instant>; 6],
    /// Guarding (Shift), as the server last said.
    pub guard: bool,
    /// Seconds left of the dodge roll playing (no new roll until it ends), as of `at`.
    pub dodge_cd: f32,
    /// Carries a Z-Crystal, so the Z-Move can be cast. A server older than Z-Crystals never says:
    /// its Z-Moves need none.
    pub zcrystal: bool,
    /// Gliding (`flight`), as the server last said: the bar drains between updates.
    pub gliding: bool,
}

impl Me {
    pub fn cooldown_now(&self, slot: usize) -> f32 {
        (self.cooldown[slot] - self.at.elapsed().as_secs_f32()).max(0.0)
    }

    pub fn energy_now(&self) -> f32 {
        // Gliding takes PP (the server's `flight`) instead of giving it back.
        let rate = if self.gliding { -flight::GLIDE_PP_PER_SEC } else { ENERGY_REGEN };
        (self.energy + rate * self.at.elapsed().as_secs_f32()).clamp(0.0, 100.0)
    }
}

/// A cone's half angle.
fn cone_half(mv: &Move) -> f64 {
    if mv.cone > 0.0 { f64::from(mv.cone) * 0.5 } else { CONE_HALF_DEG }.to_radians()
}

/// How long a predicted cast outranks the server's older numbers (covers a round trip plus a state interval).
const PREDICT_SECS: f32 = 0.6;

#[derive(Default)]
pub struct Client {
    /// Thunderstorm: who called it and when it ends.
    pub storm: Option<(String, Instant)>,
    /// The three highest-level players, who carry a marker (GDD 4.2).
    pub wanted: Vec<String>,
    pub species_by_name: HashMap<String, String>,
    /// Name -> (prestige total, palette skin tier), from `looks`.
    pub looks: HashMap<String, (u32, u32)>,
    /// Name -> the Mega model the player is drawn as (`charizard_megax`), from the fourth field of `looks`.
    pub megas: HashMap<String, String>,
    /// Players the server says are shiny (`shiny <name> 1|0`).
    pub shiny: HashSet<String>,
    pub me: Option<Me>,
    /// This client's own player name, learned when its body is drawn.
    pub my_name: Option<String>,
    pub casts: HashMap<String, (String, Instant)>,
    /// Each player's latest wind-up from the server's `charge`: its length and when it was heard
    /// (stand-in attack poses time themselves by it).
    pub windups: HashMap<String, (f32, Instant)>,
    /// Fainted: seconds left when told, who to watch, and when that was.
    pub ko: Option<(f32, Option<String>, Instant)>,
    /// Shift is held as a Pokémon (guard), as this client last told the server.
    pub guard_held: bool,
    /// Keys held now, so a held key's repeats aren't new presses.
    pub down: Vec<String>,
    /// The last movement key tapped and when, for the double-tap dodge.
    pub last_tap: Option<(char, Instant)>,
    /// Dodge rolls playing: player name -> direction (f, b, l, r) and when it started.
    pub rolls: HashMap<String, (char, Instant)>,
    /// Every Pokémon player's level, HP and max HP, for the battle box over its head.
    pub plates: HashMap<String, (u32, f32, f32)>,
    /// NPCs near this player, by entity id (`npcs`).
    pub npcs: HashMap<u32, NpcView>,
    /// NPCs drawn bigger than their species (Alphas): entity id -> size.
    pub npc_sizes: HashMap<u32, f32>,
    /// Alphas (`npcalpha`): drawn with an aura.
    pub npc_alphas: HashSet<u32>,
    /// This client's own move winding up, until when (it moves at 60% and can't cast another).
    pub charge_until: Option<Instant>,
    /// `/capi learn`: move ids in place of the kit's, slot by slot.
    pub learned: [Option<String>; 4],
    /// This client's species and when it came out as it (Fake Out works for 3 s).
    pub since: Option<(String, Instant)>,
    /// Minimize: player name -> drawn scale, from when, for how long (seconds).
    pub sizes: HashMap<String, (f32, Instant, f32)>,
    /// Double Team: player name -> copies left, from when, for how long (seconds).
    pub copies: HashMap<String, (u8, Instant, f32)>,
}

/// Spec: the caster moves at 60% while a move winds up.
const CHARGE_SLOW: f32 = 0.6;

impl Client {
    fn charging(&self) -> bool {
        self.charge_until.is_some_and(|t| Instant::now() < t)
    }

    /// The move this client has in `slot`: a learned one, else its kit's.
    fn slot_move(&self, species: &str, slot: usize) -> Option<&'static Move> {
        if slot == 5 {
            return ultimate::of(species);
        }
        // An untransformed Ditto has Transform only. Nobody else has a slot 4: the signature move
        // is in the move pool and goes on LMB, Q, E or R like any other move.
        if sig::plain_ditto(species) {
            return Some(&sig::transform_sig().mv);
        }
        if slot == 4 {
            return None;
        }
        match self.learned.get(slot) {
            Some(Some(id)) => learn_move(id),
            _ => kit(species).and_then(|k| k.moves.get(slot)),
        }
    }
}

/// `looks`' list, `name:total:skin[:mega],...`: the totals and skins, and the Mega models of those who have one.
/// The Mega field is optional, so a server from before Megas still parses.
fn parse_looks(list: &str) -> (HashMap<String, (u32, u32)>, HashMap<String, String>) {
    let (mut looks, mut megas) = (HashMap::new(), HashMap::new());
    for e in list.split(',') {
        let mut f = e.split(':');
        let (Some(name), Some(total), Some(skin)) = (f.next(), f.next().and_then(|n| n.parse().ok()), f.next().and_then(|n| n.parse().ok())) else { continue };
        looks.insert(name.to_string(), (total, skin));
        // Only a plain id: it names a model.
        if let Some(model) = f.next().filter(|m| !m.is_empty() && m.len() <= 48 && m.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')) {
            megas.insert(name.to_string(), model.to_string());
        }
    }
    (looks, megas)
}

/// The Mega model this player is drawn as (`charizard_megax`), if the server says it is one.
pub fn mega_of(name: &str) -> Option<String> {
    CLIENT.lock().unwrap().megas.get(name).cloned()
}

/// The palette skin this player is drawn with (0 = none).
pub fn skin_of(name: &str) -> u32 {
    CLIENT.lock().unwrap().looks.get(name).map_or(0, |l| l.1)
}

/// The name plate's text when it's more than the name: Most Wanted (GDD 4.2) and the prestige total as stars (GDD 4.7).
pub fn plate_label(name: &str) -> Option<String> {
    let c = CLIENT.lock().unwrap();
    let total = c.looks.get(name).map_or(0, |l| l.0);
    let stars = match total {
        0 => String::new(),
        1..=5 => format!("{} ", "\u{2605}".repeat(total as usize)),
        _ => format!("\u{2605}{total} "),
    };
    let wanted = if c.wanted.iter().any(|n| n == name) { "[WANTED] " } else { "" };
    (total > 0 || !wanted.is_empty()).then(|| format!("{wanted}{stars}{name}"))
}

/// Whether this player is drawn with its species' shiny texture.
pub fn shiny_of(name: &str) -> bool {
    CLIENT.lock().unwrap().shiny.contains(name)
}

/// How big this player is drawn (Minimize), 1 normally.
pub fn size_of(name: &str) -> f32 {
    let c = CLIENT.lock().unwrap();
    c.sizes.get(name).filter(|(_, at, secs)| at.elapsed().as_secs_f32() < *secs).map_or(1.0, |(s, _, _)| *s)
}

/// How many Double Team copies stand around this player now.
pub fn copies_of(name: &str) -> u8 {
    let c = CLIENT.lock().unwrap();
    c.copies.get(name).filter(|(_, at, secs)| at.elapsed().as_secs_f32() < *secs).map_or(0, |(n, _, _)| *n)
}

/// The battle box numbers for this player: level, HP, max HP.
pub fn plate_of(name: &str) -> Option<(u32, f32, f32)> {
    CLIENT.lock().unwrap().plates.get(name).copied()
}

/// How long a dodge roll's tumble lasts on screen (seconds).
pub const ROLL_SECS: f32 = 0.35;

/// A dodge roll playing for this player: its direction and how far through it is (0-1).
pub fn roll_of(name: &str) -> Option<(char, f32)> {
    let c = CLIENT.lock().unwrap();
    let (code, at) = c.rolls.get(name)?;
    let t = at.elapsed().as_secs_f32() / ROLL_SECS;
    (t < 1.0).then_some((*code, t))
}

/// A player's cast for a stand-in attack pose: the animation name (`physical`, `special`, ...),
/// seconds since it started and its wind-up (0 when the server sent none: an instant move, or a
/// cast this client predicted whose `charge` hasn't come yet). Kept for `within(windup)` seconds.
pub fn cast_pose_of(name: &str, within: impl Fn(f32) -> f32) -> Option<(String, f32, f32)> {
    let c = CLIENT.lock().unwrap();
    let (anim, at) = c.casts.get(name)?;
    let e = at.elapsed().as_secs_f32();
    // The wind-up that belongs to this cast: heard just before it (others: `cast` and `charge`
    // come in the same tick) or after it (our own key press is predicted).
    let windup = c
        .windups
        .get(name)
        .filter(|(_, heard)| {
            let d = heard.saturating_duration_since(*at).as_secs_f32() - at.saturating_duration_since(*heard).as_secs_f32();
            (-0.2..0.6).contains(&d)
        })
        .map_or(0.0, |(t, _)| *t);
    (e < within(windup)).then(|| (anim.clone(), e, windup))
}

/// A nearby NPC as the server last described it, and the way it faced before that.
#[derive(Clone, Copy, Debug)]
pub struct NpcView {
    pub level: u32,
    pub hp: f32,
    pub max: f32,
    pub yaw: f32,
    pub was: f32,
    pub moving: bool,
    pub at: Instant,
}

impl NpcView {
    /// Turned smoothly over the quarter second until the next update.
    pub fn facing(&self) -> f32 {
        let t = (self.at.elapsed().as_secs_f32() / 0.25).min(1.0);
        self.was + ((self.yaw - self.was + 540.0).rem_euclid(360.0) - 180.0) * t
    }
}

pub fn npc_size(id: u32) -> f32 {
    CLIENT.lock().unwrap().npc_sizes.get(&id).copied().unwrap_or(1.0)
}

pub fn npc_alpha(id: u32) -> bool {
    CLIENT.lock().unwrap().npc_alphas.contains(&id)
}

pub fn npc_view(id: u32) -> Option<NpcView> {
    CLIENT.lock().unwrap().npcs.get(&id).copied()
}

/// How high this client's Pokémon steps: 1 block, 2 for big ones, a person's 0.6 otherwise.
pub fn step_height() -> f32 {
    let c = CLIENT.lock().unwrap();
    match c.me.as_ref().and_then(|m| kit(&m.species)) {
        Some(k) => step_for(k.body),
        None => 0.6,
    }
}

const BIG_BODY: f32 = 2.5;

pub fn step_for(body: f32) -> f32 {
    if body >= BIG_BODY { 2.0 } else { 1.0 }
}

pub static CLIENT: LazyLock<Mutex<Client>> = LazyLock::new(Default::default);

/// `<id>,<size>;...`
fn parse_npc_sizes(list: &str) -> HashMap<u32, f32> {
    list.split(';').filter_map(|e| { let (id, size) = e.split_once(',')?; Some((id.parse().ok()?, size.parse().ok()?)) }).collect()
}

/// A `state` line (prefix: later servers add fields on the end; an older one has no slot C field).
fn parse_state(f: &[&str]) -> Option<Me> {
    let ["state", species, level, hp, max, energy, c1, c2, c3, c4, rest @ ..] = f else { return None };
    let n = |s: &str| s.parse::<f32>().unwrap_or(0.0);
    Some(Me {
        species: species.to_string(),
        level: level.parse().unwrap_or(1),
        hp: n(hp),
        max_hp: n(max),
        energy: n(energy),
        cooldown: [n(c1), n(c2), n(c3), n(c4), rest.get(2).map_or(0.0, |s| n(s)), rest.get(3).map_or(0.0, |s| n(s))],
        at: Instant::now(),
        predicted: [None; 6],
        second: None,
        rolled: None,
        guard: rest.first() == Some(&"1"),
        dodge_cd: rest.get(1).map_or(0.0, |s| n(s)),
        zcrystal: rest.get(4) != Some(&"0"),
        gliding: false,
    })
}

pub fn net_from_server(host: &mut dyn Host, event: &mut NetFromServerEvent<'_>) {
    if event.channel != CHANNEL {
        return;
    }
    let text = String::from_utf8_lossy(event.data).to_string();
    let f: Vec<&str> = text.split('\t').collect();
    if crate::emotes::heard(&f) {
        return;
    }
    if let ["proto", _] = f.as_slice() {
        // The server asks which wire version this client speaks; it decides whether we may stay.
        let _ = host.net_send_to_server(CHANNEL, format!("hello	{PROTOCOL}").as_bytes());
        return;
    }
    if let ["ping", seq] = f.as_slice() {
        // Answered at once: the server times the round trip.
        let _ = host.net_send_to_server(CHANNEL, format!("pong	{seq}").as_bytes());
        return;
    }
    sig::heard(host, &f);
    onboard::guide::hello(host, &f);
    onboard::launch_ui::hello(host, &f);
    if onboard::heard(&f) {
        // A menu this line opened may have closed a server one: tell the server now.
        onboard::menus::flush(host);
        return;
    }
    if flight::heard(host, &f) {
        return;
    }
    let mut c = CLIENT.lock().unwrap();
    if crate::fx::heard(&f, c.my_name.as_deref()) {
        return;
    }
    match f.as_slice() {
        ["species", name, species] => {
            if *species == "-" {
                c.species_by_name.remove(*name);
                c.shiny.remove(*name);
                if c.my_name.as_deref() == Some(*name) {
                    c.me = None;
                }
            } else {
                c.species_by_name.insert(name.to_string(), species.to_string());
            }
        }
        ["looks", list] => {
            let (looks, megas) = parse_looks(list);
            c.looks = looks;
            c.megas = megas;
        }
        ["shiny", name, on] => {
            if *on == "1" {
                c.shiny.insert(name.to_string());
            } else {
                c.shiny.remove(*name);
            }
        }
        ["state", "-"] => {
            c.me = None;
            c.since = None;
        }
        ["learned", rest @ ..] => {
            for i in 0..4 {
                c.learned[i] = rest.get(i).filter(|id| **id != "-" && learn_move(id).is_some()).map(|id| id.to_string());
            }
        }
        ["size", name, scale, secs, ..] => {
            let scale = scale.parse::<f32>().unwrap_or(1.0).clamp(0.2, 3.0);
            let secs = secs.parse::<f32>().unwrap_or(0.0).clamp(0.0, 30.0);
            c.sizes.insert(name.to_string(), (scale, Instant::now(), secs));
        }
        ["copies", name, n, secs, ..] => {
            let n = n.parse::<u8>().unwrap_or(0).min(3);
            let secs = secs.parse::<f32>().unwrap_or(0.0).clamp(0.0, 30.0);
            c.copies.insert(name.to_string(), (n, Instant::now(), secs));
        }
        // A prefix: later servers may add fields on the end.
        ["state", _, _, _, _, _, _, _, _, _, ..] => {
            let Some(mut me) = parse_state(&f) else { return };
            me.gliding = c.my_name.as_deref().is_some_and(flight::gliding);
            // Keep a fresh prediction over a state packet the server sent before it saw the cast.
            if let Some(old) = &c.me {
                for i in 0..6 {
                    if let Some(t) = old.predicted[i].filter(|t| t.elapsed().as_secs_f32() < PREDICT_SECS) {
                        if old.cooldown_now(i) > me.cooldown[i] {
                            me.cooldown[i] = old.cooldown_now(i);
                            me.energy = me.energy.min(old.energy_now());
                        }
                        me.predicted[i] = Some(t);
                    }
                }
                // The same for a roll: its PP stays spent and its lock stays on.
                if let Some(t) = old.rolled.filter(|t| t.elapsed().as_secs_f32() < PREDICT_SECS) {
                    me.energy = me.energy.min(old.energy_now());
                    me.dodge_cd = me.dodge_cd.max(old.dodge_cd - old.at.elapsed().as_secs_f32());
                    me.rolled = Some(t);
                }
            }
            // Came out (or as another species): Fake Out's 3 s start now.
            if c.since.as_ref().is_none_or(|(s, _)| s != &me.species) {
                c.since = Some((me.species.clone(), Instant::now()));
            }
            c.me = Some(me);
        }
        ["ko", secs, trainer] => {
            let secs = secs.parse::<f32>().unwrap_or(0.0);
            if secs <= 0.0 {
                c.ko = None;
            } else {
                // Only the fainted player gets this, so it clears the HUD even before `my_name` is known.
                c.me = None;
                c.since = None;
                c.ko = Some((secs, Some(trainer.to_string()).filter(|t| t != "-"), Instant::now()));
            }
        }
        ["npcs", list, ..] => {
            let now = Instant::now();
            let old = std::mem::take(&mut c.npcs);
            for e in list.split(';') {
                let v: Vec<&str> = e.split(',').collect();
                let [id, level, hp, max, yaw, moving] = v.as_slice() else { continue };
                let Ok(id) = id.parse::<u32>() else { continue };
                let n = |s: &str| s.parse::<f32>().unwrap_or(0.0);
                let was = old.get(&id).map_or(n(yaw), NpcView::facing);
                c.npcs.insert(id, NpcView { level: level.parse().unwrap_or(1), hp: n(hp), max: n(max), yaw: n(yaw), was, moving: *moving == "1", at: now });
            }
        }
        ["npcsizes", list, ..] => {
            c.npc_sizes = parse_npc_sizes(list);
        }
        ["npcalpha", list, ..] => {
            c.npc_alphas = list.split(';').filter_map(|s| s.parse().ok()).collect();
        }
        ["plates", list, ..] => {
            c.plates.clear();
            for e in list.split(';') {
                let v: Vec<&str> = e.split(',').collect();
                if let [name, level, hp, max] = v.as_slice() {
                    let n = |s: &str| s.parse::<f32>().unwrap_or(0.0);
                    c.plates.insert(name.to_string(), (level.parse().unwrap_or(1), n(hp), n(max)));
                }
            }
        }
        // charge <who> <move> <type> <secs> ...: a wind-up started; charge <who> -: interrupted.
        ["charge", name, rest @ ..] => {
            match (rest.first(), rest.get(2).and_then(|s| s.parse::<f32>().ok())) {
                (Some(&"-"), _) | (_, None) => {
                    c.windups.remove(*name);
                }
                (_, Some(t)) => {
                    c.windups.insert(name.to_string(), (t.clamp(0.0, 5.0), Instant::now()));
                }
            }
            if c.my_name.as_deref() == Some(*name) {
                let secs = rest.get(2).and_then(|s| s.parse::<f32>().ok()).filter(|t| t.is_finite());
                c.charge_until = match (rest.first(), secs) {
                    (Some(&"-"), _) | (_, None) => None,
                    (_, Some(t)) => Some(Instant::now() + std::time::Duration::from_secs_f32(t.clamp(0.0, 5.0))),
                };
            }
        }
        // push <x> <y> <z>: the server moves this player (a dash's exit momentum, a knockback).
        ["storm", name, secs, ..] => {
            let s = finite_secs(secs, 30.0);
            c.storm = Some((name.to_string(), Instant::now() + std::time::Duration::from_secs_f32(s)));
        }
        ["wanted", names] => {
            c.wanted = names.split(',').filter(|n| !n.is_empty()).map(str::to_string).collect();
        }
        ["push", x, y, z, ..] => {
            let n = |s: &str| s.parse::<f32>().ok().filter(|v| v.is_finite()).unwrap_or(0.0).clamp(-4.0, 4.0);
            let kick = [n(x), n(y), n(z)];
            drop(c);
            host.push_player(kick);
        }
        ["roll", name, code, ..] => {
            let code = code.chars().next().unwrap_or('f');
            c.rolls.retain(|_, (_, at)| at.elapsed().as_secs_f32() < ROLL_SECS);
            c.rolls.insert(name.to_string(), (code, Instant::now()));
        }
        ["cast", name, anim] => {
            // Our own cast already played on the key press.
            let mine = c.my_name.as_deref() == Some(*name)
                && c.casts.get(*name).is_some_and(|(_, at)| at.elapsed().as_secs_f32() < PREDICT_SECS);
            if !mine {
                c.casts.insert(name.to_string(), (anim.to_string(), Instant::now()));
            }
        }
        _ => {}
    }
}

/// The species a player is drawn as, and the cast animation playing, if any.
pub fn look_of(name: &str, local: bool) -> Option<(String, Option<String>)> {
    let mut c = CLIENT.lock().unwrap();
    if local && c.my_name.as_deref() != Some(name) {
        c.my_name = Some(name.to_string());
    }
    let species = c.species_by_name.get(name)?.clone();
    let anim = c
        .casts
        .get(name)
        .filter(|(_, at)| at.elapsed().as_secs_f32() < CAST_ANIM_SECS)
        .map(|(a, _)| a.clone());
    Some((species, anim))
}

/// A player's sprint in blocks a second, for the speeds the GDD states outright.
const SPRINT_BLOCKS: f32 = 5.6;

/// Two taps of a movement key within this long make a dodge roll.
const DOUBLE_TAP_SECS: f32 = 0.3;

pub const TAB: &str = "key.keyboard.tab";

/// Whether the window had focus at the last GUI frame (`gui_render_post`).
static WAS_FOCUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Tab must leave `down` when it's let go: while it's there the player list layer is cancelled,
/// and with it every NPC and player skin (`layer_cancelled` in lib.rs). `key` doesn't always hear
/// the release: chat, signs, books, dialogs and prompts eat it, and an unfocused window gets none.
/// So a real Tab release (`input_key`, every window key event) forgets it, and so does losing
/// focus (only the moment it's lost: a window that never had it, as a test bot's, keeps its keys).
pub fn forget_tab(down: &mut Vec<String>, tab_released: bool, was_focused: bool, focused: bool) {
    if tab_released || (was_focused && !focused) {
        down.retain(|k| k != TAB);
    }
}

/// `forget_tab` for a real window key event.
pub fn tab_released(released: bool) {
    if released {
        forget_tab(&mut CLIENT.lock().unwrap().down, true, true, true);
    }
}

/// A real Shift release (`input_key`, every window key event): the escape menu, chat and other
/// screens eat it before `key` hears it, which left guard stuck on (Onii, 2026-11-20). Forget the
/// key and drop the guard.
pub fn shift_released(host: &mut dyn Host) {
    let mut c = CLIENT.lock().unwrap();
    c.down.retain(|k| !k.ends_with(".shift"));
    if c.guard_held {
        c.guard_held = false;
        drop(c);
        let _ = host.net_send_to_server(CHANNEL, b"guard	0");
    }
}

/// `forget_tab` once a GUI frame, with the window's focus.
pub fn focus_seen(focused: bool) {
    let was = WAS_FOCUSED.swap(focused, std::sync::atomic::Ordering::Relaxed);
    if was && !focused {
        forget_tab(&mut CLIENT.lock().unwrap().down, false, was, focused);
    }
}

/// GDD 3.1 keys for a Pokémon: LMB (see `left_click`), Q, E, R are moves 1-4, Z the Z-Move; F only
/// interacts (`onboard::key` sends `use`), Tab only opens the menu (`hub`). Shift guards while held,
/// and a double-tapped W, A, S or D is a dodge roll that way.
pub fn key(host: &mut dyn Host, event: &mut KeyEvent<'_>) {
    let mut c = CLIENT.lock().unwrap();
    // A held key's repeats aren't new presses.
    let repeat = event.pressed && c.down.iter().any(|k| k == event.key);
    if event.pressed && !repeat && host.playing() {
        drop(c);
        let taken = onboard::key(host, event.key);
        // A held W/S on a list repeats (`menus::tick`); any other key stops that.
        onboard::menus::pressed(event.key);
        c = CLIENT.lock().unwrap();
        if taken {
            c.down.push(event.key.to_string());
            return;
        }
    }
    if event.pressed {
        if !repeat {
            c.down.push(event.key.to_string());
        }
    } else {
        c.down.retain(|k| k != event.key);
        drop(c);
        onboard::menus::released(event.key);
        onboard::progress::key_up(host, event.key);
        onboard::catch::key_up(host, event.key);
        c = CLIENT.lock().unwrap();
    }
    let mon = c.me.is_some();
    if matches!(event.key, "key.keyboard.left.shift" | "key.keyboard.right.shift") {
        let on = mon && host.playing() && !onboard::menus::any_open() && c.down.iter().any(|k| k.ends_with(".shift"));
        if on != c.guard_held {
            c.guard_held = on;
            drop(c);
            let _ = host.net_send_to_server(CHANNEL, format!("guard\t{}", u8::from(on)).as_bytes());
        }
        return;
    }
    // No moves, Z-Move or dodge while a menu is up (`onboard::key` takes those keys too).
    if !event.pressed || repeat || !mon || !host.playing() || onboard::menus::any_open() {
        return;
    }
    // Space: a vanilla jump; a quick second one takes a winged Pokémon up (`flight`).
    if event.key == "key.keyboard.space" {
        let (species, me) = (c.me.as_ref().map(|m| m.species.clone()).unwrap_or_default(), c.my_name.clone());
        drop(c);
        flight::space(host, &species, me.as_deref());
        return;
    }
    // GDD 3.1 keybinds: LMB (see `left_click`), Q, E, R are the four moves and Z the Z-Move. No other
    // key casts (no number keys, no signature key: the signature move sits in one of the four).
    let slot = match event.key {
        "key.keyboard.q" => Some(1),
        "key.keyboard.e" => Some(2),
        "key.keyboard.r" => Some(3),
        "key.keyboard.z" => Some(5),
        _ => None,
    };
    if let Some(slot) = slot {
        drop(c);
        cast_slot(host, slot);
        return;
    }
    let dir = match event.key {
        "key.keyboard.w" => 'f',
        "key.keyboard.s" => 'b',
        "key.keyboard.a" => 'l',
        "key.keyboard.d" => 'r',
        _ => return,
    };
    let double = c.last_tap.is_some_and(|(d, at)| d == dir && at.elapsed().as_secs_f32() < DOUBLE_TAP_SECS);
    if !double {
        c.last_tap = Some((dir, Instant::now()));
        return;
    }
    c.last_tap = None;
    // Ask only for a roll the server will give, and take its PP off the bar at once.
    let ready = c.me.as_mut().is_some_and(|m| {
        let (energy, lock) = (m.energy_now(), m.dodge_cd - m.at.elapsed().as_secs_f32());
        if !can_dodge(energy, lock) {
            return false;
        }
        for i in 0..6 {
            m.cooldown[i] = m.cooldown_now(i);
        }
        m.at = Instant::now();
        m.energy = energy - DODGE_COST;
        m.dodge_cd = DODGE_LOCK;
        m.rolled = Some(Instant::now());
        true
    });
    drop(c);
    if ready {
        let _ = host.net_send_to_server(CHANNEL, format!("dodge\t{dir}").as_bytes());
    }
}

/// A Pokémon doesn't crouch (Shift is guard), and moves at half speed while guarding (GDD 3.1).
pub fn movement_input(host: &dyn Host, event: &mut MovementInputEvent) {
    // Photo mode first: its keys fly the camera, so the Pokémon gets none of them.
    onboard::photo::movement_input(event);
    // Any menu up: standing still, held keys included (play-test #8).
    onboard::menus::movement_input(event);
    onboard::catch::movement_input(event);
    let c = CLIENT.lock().unwrap();
    if c.me.is_none() {
        return;
    }
    event.sneak = false;
    if c.guard_held {
        event.forward *= 0.5;
        event.strafe *= 0.5;
    }
    if c.charging() {
        event.forward *= CHARGE_SLOW;
        event.strafe *= CHARGE_SLOW;
    }
    if let Some(me) = c.my_name.as_deref() {
        let k = c.me.as_ref().and_then(|m| kit(&m.species));
        let fixed = k.and_then(|k| k.fixed_speed).map_or(1.0, |v| v / SPRINT_BLOCKS);
        // Size and Speed: the server's vanilla Speed effect does the steps, this trims to the value.
        let own = k.map_or(1.0, |k| speed_trim(move_speed(k)));
        let m = sig::move_mult(me) * fixed * own;
        event.forward *= m;
        event.strafe *= m;
    }
    drop(c);
    flight::movement_input(host, &mut event.jump);
}

/// How fast a Pokémon moves against a player's normal speed (1 = 5.6 blocks a second sprinting),
/// from its base Speed and its size: a big body covers more ground a stride. Moltres (base 90,
/// 4 blocks tall) moves about 1.55x, Pikachu 1.1x, Snorlax (base 30, big) 1.15x, Shuckle 0.8x.
pub fn move_speed(k: &Kit) -> f32 {
    let stat = 0.75 + 0.5 * k.base[5].clamp(20.0, 160.0) / 100.0;
    let size = (1.0 + 0.18 * (k.body.max(0.1) / 1.2).log2()).clamp(0.85, 1.35);
    (stat * size).clamp(0.8, 1.7)
}

/// The vanilla Speed effect's amplifier the server gives for `mult` (each level is +20%), if any.
pub fn speed_amp(mult: f32) -> Option<u8> {
    (mult > 1.001).then(|| (((mult - 1.0) / 0.2).ceil() as u8).max(1) - 1)
}

/// What the client keeps of the Speed effect's boost so the total is `mult` (at most 1: input can
/// only be scaled down).
pub fn speed_trim(mult: f32) -> f32 {
    let effect = speed_amp(mult).map_or(1.0, |a| 1.0 + 0.2 * f32::from(a + 1));
    (mult / effect).min(1.0)
}

/// Each frame (`lib.rs`): menu bookkeeping (`onboard::menus::tick`). A menu opening drops the
/// guard and a half-done double tap; closing it with Shift still held guards again.
pub fn menus_tick(host: &mut dyn Host) {
    let Some(open) = onboard::menus::tick(host) else { return };
    let mut c = CLIENT.lock().unwrap();
    c.last_tap = None;
    let on = !open && c.me.is_some() && host.playing() && c.down.iter().any(|k| k.ends_with(".shift"));
    if on != c.guard_held {
        c.guard_held = on;
        drop(c);
        let _ = host.net_send_to_server(CHANNEL, format!("guard\t{}", u8::from(on)).as_bytes());
    }
}

/// The connection ended (or the world closed): this client is nobody's Pokémon any more. The
/// server drops a player's Pokémon when they log out, so the next world starts as a trainer.
pub fn forget() {
    *CLIENT.lock().unwrap() = Client::default();
    flight::forget();
    onboard::forget();
    sig::forget_client();
}

/// Whether this client plays as a Pokémon now (or is fainted, which comes back as one).
pub fn is_pokemon() -> bool {
    let c = CLIENT.lock().unwrap();
    c.me.is_some() || c.ko.is_some()
}

/// GDD 3.1: a Pokémon's left click is its basic attack (slot 1), never a vanilla swing or block
/// break. True when the click was taken (the caller cancels it).
pub fn left_click(host: &mut dyn Host) -> bool {
    if CLIENT.lock().unwrap().me.is_none() {
        return false;
    }
    if onboard::click_skips(host) {
        return true;
    }
    cast_slot(host, 0);
    true
}

/// Predict and send a cast of `slot` (0-3), if this client is a Pokémon and the move is ready.
/// Where the camera is now, when the shoulder camera has posed it recently.
pub fn cam_now() -> Option<[f64; 3]> {
    CAM.lock().unwrap().posed.filter(|(_, at)| at.elapsed().as_secs_f32() < 0.5).map(|(p, _)| p)
}

/// Thunderstorm: this client is in the dark now (never the Zapdos who called it).
pub fn in_storm() -> bool {
    let c = CLIENT.lock().unwrap();
    c.storm.as_ref().is_some_and(|(n, until)| Instant::now() < *until && c.my_name.as_deref() != Some(n))
}

fn cast_slot(host: &mut dyn Host, slot: usize) {
    let mut c = CLIENT.lock().unwrap();
    // One move at a time: the server refuses a cast while another winds up.
    if c.charging() {
        return;
    }
    let Some(species) = c.me.as_ref().map(|m| m.species.clone()) else { return };
    let slot = if sig::plain_ditto(&species) && slot != 5 { 4 } else { slot };
    let Some(mv) = c.slot_move(&species, slot) else { return };
    // Fake Out only in the first 3 s after coming out (the server says why it refused).
    let early = c.since.as_ref().is_some_and(|(_, at)| at.elapsed().as_secs_f32() < FAKE_OUT_TICKS as f32 / 20.0);
    if mv.id == "fakeout" && !early {
        drop(c);
        let _ = host.net_send_to_server(CHANNEL, format!("cast\t{slot}").as_bytes());
        return;
    }
    let Some(me) = c.me.as_mut() else { return };
    // No Z-Crystal: nothing to predict; the server says what's missing.
    if slot == 5 && !me.zcrystal {
        drop(c);
        let _ = host.net_send_to_server(CHANNEL, format!("cast\t{slot}").as_bytes());
        return;
    }
    // Predict only what the server will accept, so the animation never plays for a refused cast.
    let free = mv.charges > 1 && me.second.is_some_and(|(s, at)| s == slot && at.elapsed().as_secs_f32() < ultimate::SECOND_TICKS as f32 / 20.0);
    if !free && (me.cooldown_now(slot) > 0.0 || me.energy_now() < mv.energy) {
        return;
    }
    // Rebase the countdowns on now, then apply the cast.
    let energy = me.energy_now();
    for i in 0..6 {
        me.cooldown[i] = me.cooldown_now(i);
    }
    me.at = Instant::now();
    if free {
        me.energy = energy;
        me.second = None;
    } else {
        me.energy = energy - mv.energy;
        me.cooldown[slot] = mv.cooldown;
        me.predicted[slot] = Some(Instant::now());
        if mv.charges > 1 {
            me.second = Some((slot, Instant::now()));
        }
    }
    let anim = mv.anim.clone();
    if let Some(name) = c.my_name.clone() {
        c.casts.insert(name, (anim, Instant::now()));
    }
    drop(c);
    let cam = CAM.lock().unwrap().posed.filter(|(_, at)| at.elapsed().as_secs_f32() < 0.5).map(|(p, _)| p);
    let msg = match cam {
        Some(p) => format!("cast\t{slot}\t{:.2}\t{:.2}\t{:.2}", p[0], p[1], p[2]),
        None => format!("cast\t{slot}"),
    };
    let _ = host.net_send_to_server(CHANNEL, msg.as_bytes());
}

/// How far behind the trainer a fainted player's camera sits.
const KO_ORBIT: f64 = 4.0;

/// The local player's camera, frame by frame: `camera_distance` (third person only) says which
/// view and where the feet are; `camera_pose` (same frame) places the camera; casts and the
/// effects pass read the result.
pub struct Cam {
    /// `camera_distance` ran this frame for the back view (not the front view, F5 twice).
    back: bool,
    /// The local player's feet at this frame's partial tick, and when.
    feet: Option<([f64; 3], Instant)>,
    /// Where the over-the-shoulder camera is this frame (none in first person or the front view).
    posed: Option<([f64; 3], Instant)>,
}

pub static CAM: Mutex<Cam> = Mutex::new(Cam { back: false, feet: None, posed: None });

/// Third person, a Pokémon: note the view and the interpolated feet for `camera_pose`.
pub fn camera_distance(event: &mut CameraDistanceEvent<'_>) {
    let p = f64::from(event.partial.clamp(0.0, 1.0));
    let e = event.entity;
    let feet = [0, 1, 2].map(|i| e.previous[i] + (e.position[i] - e.previous[i]) * p);
    let mut cam = CAM.lock().unwrap();
    cam.back = !event.mirrored;
    cam.feet = Some((feet, Instant::now()));
}

/// The local player's feet as of this frame's camera, while it's in third person.
pub fn local_feet() -> Option<[f64; 3]> {
    CAM.lock().unwrap().feet.filter(|(_, at)| at.elapsed().as_secs_f32() < 0.1).map(|(p, _)| p)
}

/// Fainted: the camera orbits the trainer. A Pokémon in third person (back view): over its right
/// shoulder, sized to the species.
pub fn camera_pose(host: &dyn Host, event: &mut CameraPoseEvent<'_>) {
    let (ko, species) = {
        let c = CLIENT.lock().unwrap();
        (c.ko.as_ref().map(|k| k.1.clone()), c.me.as_ref().map(|m| m.species.clone()))
    };
    let back = std::mem::take(&mut CAM.lock().unwrap().back);
    if let Some(trainer) = ko {
        CAM.lock().unwrap().posed = None;
        if let Some(t) = trainer {
            ko_orbit(host, event, &t);
        }
        return;
    }
    let posed = match species {
        Some(s) if back => shoulder(host, event, &s),
        _ => None,
    };
    // The wardrobe's own camera; moves don't aim from it.
    if back && wardrobe_cam(host, event) {
        CAM.lock().unwrap().posed = None;
        return;
    }
    if WARDROBE_CAM.lock().unwrap().take().is_some() {
        WARD_ORBIT.lock().unwrap().take();
        cosmetics::draw::reset_zoom();
    }
    CAM.lock().unwrap().posed = posed.map(|p| (p, Instant::now()));
}

/// The view's yaw and pitch when the wardrobe camera took over: the mouse orbits it from there.
static WARD_ORBIT: Mutex<Option<(f32, f32)>> = Mutex::new(None);

/// Where a ray from `o` along `d` first meets one of `quads`, between `t0` and `t1`.
fn first_hit(quads: &[[glam::Vec3; 4]], o: glam::Vec3, d: glam::Vec3, t0: f32, t1: f32) -> Option<f32> {
    let tri = |a: glam::Vec3, b: glam::Vec3, c: glam::Vec3| -> Option<f32> {
        let (e1, e2) = (b - a, c - a);
        let p = d.cross(e2);
        let det = e1.dot(p);
        if det.abs() < 1e-7 {
            return None;
        }
        let inv = 1.0 / det;
        let s = o - a;
        let u = s.dot(p) * inv;
        if !(0.0..=1.0).contains(&u) {
            return None;
        }
        let q = s.cross(e1);
        let v = d.dot(q) * inv;
        if v < 0.0 || u + v > 1.0 {
            return None;
        }
        let t = e2.dot(q) * inv;
        (t > t0 && t < t1).then_some(t)
    };
    quads.iter().filter_map(|q| [tri(q[0], q[1], q[2]), tri(q[0], q[2], q[3])].into_iter().flatten().reduce(f32::min)).reduce(f32::min)
}

/// The direction a camera at `yaw`/`pitch` looks along, as a vector.
fn look_vec(yaw: f32, pitch: f32) -> glam::Vec3 {
    let d = dir(yaw, pitch);
    glam::Vec3::new(d[0] as f32, d[1] as f32, d[2] as f32)
}

/// Where the wardrobe camera was last frame (position, yaw, pitch, when), to ease toward the next.
static WARDROBE_CAM: Mutex<Option<([f64; 3], f32, f32, Instant)>> = Mutex::new(None);

/// The wardrobe open (third person): the camera turns to face the Pokémon and sets it right of
/// centre, clear of the panel on the left. Hats frame the head with the hat on it, auras the whole
/// body with room around it, trails from behind and above, over the ground the trail falls on.
/// Everything framed comes from the body as drawn this frame (`cosmetics::draw::note_local`: the
/// posed head bone, the hat's own corners, the posed body's box), so a flying idle, a long neck or
/// a big model are framed where they really are. The camera's line to the head stays off the body
/// (a wing, a crest): it swings round to the nearest clear angle, or comes in short of what's in
/// the way. The mouse orbits it, the wheel zooms. It eases over from wherever it was. False when the
/// wardrobe is shut or the body hasn't been drawn yet.
fn wardrobe_cam(host: &dyn Host, event: &mut CameraPoseEvent<'_>) -> bool {
    use glam::{Mat3, Vec3};
    let Some(slot) = cosmetics::draw::framing() else { return false };
    let Some(body) = cosmetics::draw::local_body() else { return false };
    let Some(feet) = local_feet() else { return false };
    let (half_w, half_h) = host.camera().map_or((1.24, 0.7), |c| {
        let (x, y) = (c.projection[0], c.projection[5]);
        (if x.abs() > 1e-3 { 1.0 / f64::from(x.abs()) } else { 1.24 }, if y.abs() > 1e-3 { 1.0 / f64::from(y.abs()) } else { 0.7 })
    });
    // How far the mouse has turned the view since the wardrobe camera took over.
    let (dyaw, dpitch) = {
        let mut o = WARD_ORBIT.lock().unwrap();
        let (y0, p0) = *o.get_or_insert((event.yaw, event.pitch));
        (((event.yaw - y0 + 540.0).rem_euclid(360.0)) - 180.0, event.pitch - p0)
    };
    let face = body.yaw;
    // The body's frame to the world's turn, and back.
    let turn = Mat3::from_rotation_y((-face).to_radians());
    let (lo, hi) = body.body;
    let mid = (lo + hi) * 0.5;
    let size = hi - lo;
    // (yaw and pitch from the body's facing, the point framed in the body's frame, the radius to fit)
    let (base_yaw, base_pitch, focus_b, r) = match slot {
        cosmetics::Slot::Hat => (180.0 + 25.0, 12.0, body.head, body.head_r * 1.5),
        cosmetics::Slot::Aura => (180.0 + 30.0, 10.0, mid, size.length() * 0.5 * 0.85 + 0.35),
        cosmetics::Slot::Trail => {
            let back = (body.radius + 1.2).min(4.0);
            (25.0, 32.0, Vec3::new(mid.x, lo.y + size.y * 0.3, lo.z - back + 0.6), ((size.y * 1.2).max(2.6) + 0.6) * 0.5)
        }
    };
    let want_dist = ((f64::from(r) / half_h).max(f64::from(r) * 1.2 + 0.3) * f64::from(cosmetics::draw::zoom())).max(0.5) as f32;
    // From the focus toward the camera, in the body's frame: try the asked-for angle first, then
    // ever wider swings round the head, and take the first whose line is clear of the body.
    let (yaw0, pitch0) = (face + base_yaw + dyaw, (base_pitch + dpitch).clamp(-35.0, 75.0));
    // The camera also sits out to its left (below), so the line it really sees the focus along
    // leans that way, and is longer by `lean` for the same distance.
    let k = (0.4 * half_w) as f32;
    let lean = (1.0 + k * k).sqrt();
    let back_of = |y: f32, p: f32| {
        let ly = y.to_radians();
        turn.transpose() * ((-look_vec(y, p) + Vec3::new(ly.cos(), 0.0, ly.sin()) * k).normalize())
    };
    let near = match slot {
        cosmetics::Slot::Hat => body.head_r * 1.05,
        _ => r * 0.9,
    };
    const TRIES: [(f32, f32); 13] = [(0.0, 0.0), (15.0, 0.0), (-15.0, 0.0), (0.0, 15.0), (30.0, 5.0), (-30.0, 5.0), (0.0, 30.0), (45.0, 10.0), (-45.0, 10.0), (60.0, 20.0), (-60.0, 20.0), (90.0, 25.0), (-90.0, 25.0)];
    let mut best: Option<(f32, f32, f32)> = None;
    for (dy, dp) in TRIES {
        let (y, p) = (yaw0 + dy, (pitch0 + dp).clamp(-35.0, 80.0));
        match first_hit(&body.quads, focus_b, back_of(y, p), near, want_dist * lean) {
            None => {
                best = Some((y, p, want_dist));
                break;
            }
            Some(t) if best.is_none_or(|b| t / lean > b.2) => best = Some((y, p, t / lean)),
            Some(_) => {}
        }
    }
    // Every angle stopped close in: the head sits inside a shell of the body's own (fuzz, a
    // shell), so look through it from the asked-for angle rather than from inside it.
    let best = best.filter(|b| b.2 >= want_dist * 0.5);
    let (yaw, pitch, dist) = best.map_or((yaw0, pitch0, want_dist), |(y, p, t)| (y, p, if t < want_dist { (t - 0.12 / lean).max(near / lean + 0.05) } else { t }));
    let dist = f64::from(dist);
    let f = turn * focus_b;
    let focus = [feet[0] + f64::from(f.x), feet[1] + f64::from(f.y), feet[2] + f64::from(f.z)];
    let look = dir(yaw, pitch);
    // Out to the camera's left by 40% of the half-width, so the body sits right of centre.
    let ly = f64::from(yaw).to_radians();
    let left = [ly.cos(), 0.0, ly.sin()];
    let shift = dist * 0.4 * half_w;
    let want = [focus[0] - look[0] * dist + left[0] * shift, focus[1] - look[1] * dist, focus[2] - look[2] * dist + left[2] * shift];
    let want = clear_to(host, focus, want);
    let mut last = WARDROBE_CAM.lock().unwrap();
    let now = Instant::now();
    let (pos, y, p) = match *last {
        Some((from, fy, fp, at)) => {
            let k = 1.0 - (-(at.elapsed().as_secs_f64().min(0.25)) * 8.0).exp();
            let turn = ((yaw - fy + 540.0).rem_euclid(360.0)) - 180.0;
            ([0, 1, 2].map(|i| from[i] + (want[i] - from[i]) * k), fy + turn * k as f32, fp + (pitch - fp) * k as f32)
        }
        None => (*event.position, event.yaw, event.pitch),
    };
    *last = Some((pos, y, p, now));
    *event.position = pos;
    event.yaw = y;
    event.pitch = p;
    true
}

/// While fainted, the camera orbits the trainer, looking along the player's own (mouse-driven) view.
fn ko_orbit(host: &dyn Host, event: &mut CameraPoseEvent<'_>, trainer: &str) {
    let Some(t) = host.client_entities().into_iter().find(|e| e.player_name.as_deref() == Some(trainer)) else {
        return;
    };
    let p = f64::from(event.partial.clamp(0.0, 1.0));
    let lerp = |i: usize| t.previous_position[i] + (t.position[i] - t.previous_position[i]) * p;
    let at = [lerp(0), lerp(1) + f64::from(t.height) * 0.85, lerp(2)];
    let back = dir(event.yaw, event.pitch);
    *event.position = [at[0] - back[0] * KO_ORBIT, at[1] - back[1] * KO_ORBIT, at[2] - back[2] * KO_ORBIT];
}

/// Over the right shoulder of the player's Pokémon: pivot at its shoulder (85% of the model's
/// height), the lens a little higher, out to the right and back along the view, all scaled by the
/// model's height so a Joltik and a Wailord both see ahead. Pulled in short of any block.
fn shoulder(host: &dyn Host, event: &mut CameraPoseEvent<'_>, species: &str) -> Option<[f64; 3]> {
    let k = kit(species)?;
    let (feet, at) = CAM.lock().unwrap().feet?;
    if at.elapsed().as_secs_f32() > 0.25 {
        return None;
    }
    let h = f64::from(k.body).clamp(0.25, 9.0);
    let back = (1.0 + h * 1.6).clamp(1.4, 10.0);
    let side = (0.35 + h * 0.45).clamp(0.5, 3.2);
    let lift = 0.1 + h * 0.1;
    let pivot = [feet[0], feet[1] + h * 0.85, feet[2]];
    let yaw = f64::from(event.yaw).to_radians();
    let f = dir(event.yaw, event.pitch);
    let right = [-yaw.cos(), 0.0, -yaw.sin()];
    let shoulder = clear_to(host, pivot, [pivot[0] + right[0] * side, pivot[1] + lift, pivot[2] + right[2] * side]);
    let lens = clear_to(host, shoulder, [shoulder[0] - f[0] * back, shoulder[1] - f[1] * back, shoulder[2] - f[2] * back]);
    *event.position = lens;
    Some(lens)
}

/// The furthest point from `a` toward `b` the camera can take, stopping short of the first block
/// (with room above and below for the near plane).
fn clear_to(host: &dyn Host, a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    const PAD: f64 = 0.2;
    const STEP: f64 = 0.1;
    let d = sub(b, a);
    let len = dot(d, d).sqrt();
    if len < 1e-6 {
        return a;
    }
    let u = [d[0] / len, d[1] / len, d[2] / len];
    let at = |t: f64| [a[0] + u[0] * t, a[1] + u[1] * t, a[2] + u[2] * t];
    // A start nearer the ground (or a ceiling) than the pad, as a Joltik's head is: that side's
    // probe is solid from the first step, so it says nothing about the line; leave it out.
    let below = !solid(host, [a[0], a[1] - PAD, a[2]]);
    let above = !solid(host, [a[0], a[1] + PAD, a[2]]);
    let mut t = 0.0;
    while t <= len + PAD {
        let p = at(t);
        if solid(host, p) || (above && solid(host, [p[0], p[1] + PAD, p[2]])) || (below && solid(host, [p[0], p[1] - PAD, p[2]])) {
            return at((t - PAD - STEP).clamp(0.0, len));
        }
        t += STEP;
    }
    b
}

pub fn hud(host: &mut dyn Host, event: &mut HudEvent<'_>) {
    // Photo mode: nothing on screen but its own frame.
    if onboard::photo::hud(host, event.batch) {
        return;
    }
    // The Tab menu, over everything else.
    if onboard::hub::hud(host, event.batch) {
        return;
    }
    if onboard::guide::overlay(host, event.batch) {
        return;
    }
    // Thunderstorm: map-wide darkness (GDD 8.2).
    if in_storm() {
        let (w, h) = (event.batch.width, event.batch.height);
        rect(host, event.batch, 0.0, 0.0, w, h, 0xE000_0000);
    }
    let ko = CLIENT.lock().unwrap().ko.clone();
    if let Some((secs, trainer, at)) = ko {
        let left = (secs - at.elapsed().as_secs_f32()).max(0.0).ceil();
        let lines = [
            ("Fainted!".to_string(), 0xFF6060u32),
            (format!("Back in {left:.0}s at a Nest, full HP"), 0xFFFFFF),
            (trainer.map_or("No trainer to watch".to_string(), |t| format!("Watching {t}")), 0xC0C0C0),
        ];
        let w = lines.iter().map(|(l, _)| host.text_width(l)).fold(0.0f32, f32::max) + 8.0;
        let h = lines.len() as f32 * 10.0 + 6.0;
        let (x, y) = event.corners.place(Corner::TopLeft, w, h);
        host.quad(event.batch, [[x, y], [x + w, y], [x + w, y + h], [x, y + h]], 0x8000_0000);
        for (i, (line, rgb)) in lines.iter().enumerate() {
            host.text(event.batch, line, [x + 4.0, y + 3.0 + i as f32 * 10.0], *rgb, true);
        }
        // The KO cam keeps the team's HP on screen (GDD 2.4).
        onboard::catch::hud(host, event.batch);
        return;
    }
    if onboard::worldmap::hud(host, event.batch) {
        return;
    }
    onboard::hud(host, event.batch);
    // The PC screen shows the same details: no status panel or move cards over it.
    if onboard::pc::is_open() || onboard::tutor::is_open() || cosmetics::draw::is_open() || onboard::dex::is_open() || onboard::riftmenu::is_open() || onboard::riftmenu::revealing() {
        return;
    }
    onboard::hub::reminder(host, event);
    let Some((me, moves, my_name)) = ({
        let c = CLIENT.lock().unwrap();
        c.me.clone().map(|me| {
            // Moves 1 to 4 (plain Ditto: Transform alone), then the Z-Move (slot 5).
            let moves: Vec<(usize, &'static Move)> = (0..6).filter(|i| (*i == 4) == sig::plain_ditto(&me.species) || *i == 5).filter_map(|i| c.slot_move(&me.species, i).map(|m| (i, m))).collect();
            (me, moves, c.my_name.clone())
        })
    }) else {
        onboard::guide::hud_prompt(host, event.batch, None);
        return;
    };
    let Some(k) = kit(&me.species) else {
        onboard::guide::hud_prompt(host, event.batch, None);
        return;
    };
    // Vanilla draws no crosshair in third person; over the shoulder it marks where moves aim
    // (the centre of the screen is the camera ray `aim_point` follows).
    if CAM.lock().unwrap().posed.is_some_and(|(_, at)| at.elapsed().as_secs_f32() < 0.5) {
        let b = &mut *event.batch;
        let (cx, cy) = ((b.width / 2.0).floor(), (b.height / 2.0).floor());
        rect(host, b, cx - 5.0, cy - 1.0, 11.0, 3.0, 0x7000_0000);
        rect(host, b, cx - 1.0, cy - 5.0, 3.0, 11.0, 0x7000_0000);
        rect(host, b, cx - 4.0, cy, 9.0, 1.0, 0xE0FF_FFFF);
        rect(host, b, cx, cy - 4.0, 1.0, 9.0, 0xE0FF_FFFF);
        // Hit marker: four short diagonal ticks around the cross for a quarter second after a hit.
        if crate::fx::hit_marked() {
            for (sx, sy) in [(-1.0f32, -1.0f32), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                for i in 0..3 {
                    let d = 5.0 + i as f32;
                    rect(host, b, cx + sx * d, cy + sy * d, 2.0, 2.0, 0xF0FF_4040);
                }
            }
        }
    }
    let chips = my_name.as_deref().map_or_else(Vec::new, |n| status_chips(|kind| sig::status(n, kind).is_some()));
    hud_card(host, event, &me, k, &chips);
    let bar_end = hud_moves(host, event.batch, &me, &moves);
    // What F does goes bottom right, beside the move bar (Onii's mockup).
    onboard::guide::hud_prompt(host, event.batch, Some(bar_end.unwrap_or(0.0)));
}

/// The usual colour of each Pokémon type (RGB).
fn type_color(kind: &str) -> u32 {
    match kind {
        "normal" => 0xA8A77A,
        "fire" => 0xEE8130,
        "water" => 0x6390F0,
        "electric" => 0xF7D02C,
        "grass" => 0x7AC74C,
        "ice" => 0x96D9D6,
        "fighting" => 0xC22E28,
        "poison" => 0xA33EA1,
        "ground" => 0xE2BF65,
        "flying" => 0xA98FF3,
        "psychic" => 0xF95587,
        "bug" => 0xA6B91A,
        "rock" => 0xB6A136,
        "ghost" => 0x735797,
        "dragon" => 0x6F35FC,
        "dark" => 0x705746,
        "steel" => 0xB7B7CE,
        "fairy" => 0xD685AD,
        _ => 0x9099A1,
    }
}

const HUD_PANEL: u32 = 0xD818_1A26;

fn rect(host: &mut dyn Host, b: &mut HudBatch, x: f32, y: f32, w: f32, h: f32, argb: u32) {
    if w > 0.0 && h > 0.0 {
        host.quad(b, [[x, y], [x + w, y], [x + w, y + h], [x, y + h]], argb);
    }
}

/// A box with its corner pixels cut, which reads as rounded at GUI scale.
fn pill(host: &mut dyn Host, b: &mut HudBatch, x: f32, y: f32, w: f32, h: f32, argb: u32) {
    rect(host, b, x + 1.0, y, w - 2.0, h, argb);
    rect(host, b, x, y + 1.0, 1.0, h - 2.0, argb);
    rect(host, b, x + w - 1.0, y + 1.0, 1.0, h - 2.0, argb);
}

/// Capipimon's emblem: the capipi, a pink plush bunny (9x9 pixel art), centred on `cx, cy` and
/// `2 * r` GUI pixels wide. Drawn from `_work/branding/make_brand.py`'s MENU9 grid.
fn emblem(host: &mut dyn Host, b: &mut HudBatch, cx: f32, cy: f32, r: f32) {
    const ART: [&str; 9] = [
        ".ooo.ooo.",
        ".oio.oio.",
        "oopooopoo",
        "opxpppxpo",
        "ocpmpmpco",
        "obbbkbbbo",
        "oapwhwpdo",
        ".opwwwpo.",
        "..ooooo..",
    ];
    let px = r * 2.0 / 9.0;
    let (x0, y0) = (cx - r, cy - r);
    for (y, row) in ART.iter().enumerate() {
        for (x, c) in row.bytes().enumerate() {
            let argb = match c {
                b'a' => 0xFF72_AEE3,
                b'b' => 0xFFEC_4F8C,
                b'c' => 0xFFFF_95B5,
                b'd' => 0xFFD8_7CA0,
                b'h' => 0xFFE8_3C78,
                b'i' => 0xFFFF_7EA9,
                b'k' => 0xFFB2_2F67,
                b'm' => 0xFF16_0E12,
                b'o' => 0xFF3A_1A2C,
                b'p' => 0xFFF6_A9C4,
                b'w' => 0xFFD4_ECF8,
                b'x' => 0xFF7B_3FB8,
                _ => continue,
            };
            rect(host, b, x0 + x as f32 * px, y0 + y as f32 * px, px, px, argb);
        }
    }
}

/// `text`, cut to fit `width` GUI pixels.
fn fit(host: &dyn Host, text: &str, width: f32) -> String {
    if host.text_width(text) <= width {
        return text.to_string();
    }
    let mut t: String = text.to_string();
    while !t.is_empty() && host.text_width(&format!("{t}.")) > width {
        t.pop();
    }
    format!("{}.", t.trim_end())
}

/// The status chips beside the type badges, games-style: (label, chip colour, text colour). Bound,
/// Wet, stat stages and signature effects stay on the nameplate only.
const STATUS_CHIPS: [(&str, &str, u32, u32); 7] = [
    ("slp", "SLP", 0xFF8C_8C9C, 0xFFFFFF),
    ("par", "PAR", 0xFFF8_D030, 0x282828),
    ("brn", "BRN", 0xFFF0_8030, 0xFFFFFF),
    ("frz", "FRZ", 0xFF98_D8D8, 0x282828),
    ("psn", "PSN", 0xFFA0_40A0, 0xFFFFFF),
    // Badly poisoned shows as PSN too, as in the games.
    ("tox", "PSN", 0xFFA0_40A0, 0xFFFFFF),
    ("cnf", "CNF", 0xFFE0_70B0, 0xFFFFFF),
];

/// The chips for the statuses `on` says are on now, each label once.
fn status_chips(on: impl Fn(&str) -> bool) -> Vec<(&'static str, u32, u32)> {
    let mut out: Vec<(&'static str, u32, u32)> = Vec::new();
    for (kind, label, bg, ink) in STATUS_CHIPS {
        if on(kind) && !out.iter().any(|c| c.0 == label) {
            out.push((label, bg, ink));
        }
    }
    out
}

/// Top left: the Pokémon's card. Name and level on a coloured band, type badges and any status
/// chips, HP (green, yellow, red) and energy.
fn hud_card(host: &mut dyn Host, event: &mut HudEvent<'_>, me: &Me, k: &Kit, chips: &[(&str, u32, u32)]) {
    // The EXP bar shows when the server's XP line is about this Pokémon's level (it always is,
    // except for a borrowed form such as a legend or a ranked loaner).
    let exp = onboard::progress::exp().filter(|(lvl, _, _)| *lvl == me.level);
    let (w, h) = (160.0f32, if exp.is_some() { 61.0f32 } else { 50.0f32 });
    let (x, y) = event.corners.place(Corner::TopLeft, w, h);
    let b = &mut *event.batch;
    pill(host, b, x, y, w, h, HUD_PANEL);
    pill(host, b, x, y, w, 13.0, 0xFFB8_2A1A);
    rect(host, b, x, y + 12.0, w, 1.0, 0x6000_0000);
    emblem(host, b, x + 7.5, y + 6.5, 5.0);
    let lv = format!("Lv {}", me.level);
    let lw = host.text_width(&lv);
    host.text(b, &fit(host, &k.name, w - 26.0 - lw), [x + 15.0, y + 3.0], 0xFFFFFF, true);
    host.text(b, &lv, [x + w - 4.0 - lw, y + 3.0], 0xFFE9A0, true);

    let mut tx = x + 4.0;
    for t in &k.types {
        let label = t.to_uppercase();
        let tw = host.text_width(&label);
        pill(host, b, tx, y + 16.0, tw + 6.0, 10.0, 0xFF00_0000 | type_color(t));
        host.text(b, &label, [tx + 3.0, y + 17.0], 0xFFFFFF, true);
        tx += tw + 9.0;
    }
    // The Pokémon's own statuses, right after its types (as many as fit on the card).
    for (label, bg, ink) in chips {
        let tw = host.text_width(label);
        if tx + tw + 6.0 > x + w - 3.0 {
            break;
        }
        pill(host, b, tx, y + 16.0, tw + 6.0, 10.0, *bg);
        host.text(b, label, [tx + 3.0, y + 17.0], *ink, *ink == 0xFFFFFF);
        tx += tw + 9.0;
    }

    let frac = if me.max_hp > 0.0 { (me.hp / me.max_hp).clamp(0.0, 1.0) } else { 0.0 };
    let hp_col = if frac > 0.5 { 0xFF4C_D964 } else if frac > 0.2 { 0xFFFF_CC00 } else { 0xFFFF_3B30 };
    let hp = format!("{:.0}/{:.0}", me.hp.max(0.0), me.max_hp);
    let hw = host.text_width(&hp);
    host.text(b, "HP", [x + 4.0, y + 29.0], 0xFFD24A, true);
    let (bx, bw) = (x + 18.0, w - 26.0 - hw);
    pill(host, b, bx, y + 30.0, bw, 7.0, 0xFF26_2626);
    rect(host, b, bx + 1.0, y + 31.0, (bw - 2.0) * frac, 5.0, hp_col);
    rect(host, b, bx + 1.0, y + 31.0, (bw - 2.0) * frac, 1.0, 0x50FF_FFFF);
    host.text(b, &hp, [x + w - 4.0 - hw, y + 29.0], 0xFFFFFF, true);

    let en = me.energy_now();
    let ef = (en / 100.0).clamp(0.0, 1.0);
    let et = format!("{en:.0}");
    let ew = host.text_width(&et);
    host.text(b, "PP", [x + 4.0, y + 41.0], 0x8FD3FF, true);
    let (bx, bw) = (x + 18.0, w - 26.0 - hw.max(ew));
    pill(host, b, bx, y + 43.0, bw, 5.0, 0xFF26_2626);
    // Guarding drains PP: the bar brightens while Shift is held.
    rect(host, b, bx + 1.0, y + 44.0, (bw - 2.0) * ef, 3.0, if me.guard { 0xFFBF_E6FF } else { 0xFF3F_A9F5 });
    host.text(b, &et, [x + w - 4.0 - ew, y + 41.0], 0xFFFFFF, true);

    // Pokémon-style EXP bar along the bottom edge (Onii's mockup): no second level number.
    if let Some((_, xp, need)) = exp {
        let f = (xp as f32 / need.max(1) as f32).clamp(0.0, 1.0);
        host.text(b, "EXP", [x + 4.0, y + 51.0], 0x6FD0FF, true);
        let (bx, bw) = (x + 22.0, w - 26.0);
        rect(host, b, bx, y + 54.0, bw, 3.0, 0xFF26_2626);
        rect(host, b, bx, y + 54.0, bw * f, 3.0, 0xFF40_C8F8);
    }
}

/// Bottom centre, where the hotbar was: one card per move with its key, name, cost, and a dark
/// veil that shrinks as the cooldown runs out; then the Z-crystal for the Z-Move.
/// Returns where the bar ends on the right, when it drew.
fn hud_moves(host: &mut dyn Host, b: &mut HudBatch, me: &Me, moves: &[(usize, &Move)]) -> Option<f32> {
    let z = moves.iter().find(|(i, _)| *i == 5).map(|(_, m)| *m);
    // Not fully evolved yet: a locked socket says the Z-Move comes at the final stage (Onii 2026-11-20).
    let z_locked = z.is_none() && z_locked_for(&me.species);
    let cards: Vec<(usize, &Move)> = moves.iter().filter(|(i, _)| *i < 5).take(4).copied().collect();
    let n = cards.len();
    if n == 0 && z.is_none() && !z_locked {
        return None;
    }
    let (ch, gap) = (26.0f32, 3.0f32);
    let zw = if z.is_some() || z_locked { ch + 4.0 + gap } else { 0.0 };
    let cw = if n == 0 { 0.0 } else { ((b.width - 6.0 - zw - (n.max(1) - 1) as f32 * gap) / n as f32).floor().clamp(50.0, 80.0) };
    let total = n as f32 * cw + n.saturating_sub(1) as f32 * gap + zw;
    let x0 = ((b.width - total) / 2.0).floor();
    let y0 = b.height - ch - 3.0;
    let energy = me.energy_now();
    for (col, &(i, m)) in cards.iter().enumerate() {
        let x = x0 + col as f32 * (cw + gap);
        let cd = me.cooldown_now(i);
        let afford = energy >= m.energy;
        let ready = cd <= 0.0 && afford;
        let tc = 0xFF00_0000 | type_color(&m.kind);
        pill(host, b, x, y0, cw, ch, HUD_PANEL);
        rect(host, b, x + 1.0, y0, cw - 2.0, 2.0, tc);
        if cd > 0.0 && m.cooldown > 0.0 {
            let f = (cd / m.cooldown).clamp(0.0, 1.0);
            rect(host, b, x + cw * (1.0 - f), y0 + 2.0, cw * f, ch - 2.0, 0x9000_0000);
        }
        // Plain Ditto's Transform sits in slot 4 but goes out on the first key.
        let key = SLOT_KEYS[if i == 4 { 0 } else { i }];
        let kw = host.text_width(key) + 4.0;
        pill(host, b, x + 3.0, y0 + 4.0, kw, 10.0, if ready { tc } else { 0xFF3A_3D4D });
        host.text(b, key, [x + 5.0, y0 + 5.0], 0xFFFFFF, true);
        let name = fit(host, &m.name, cw - kw - 9.0);
        host.text(b, &name, [x + kw + 6.0, y0 + 5.0], if ready { 0xFFFFFF } else { 0xA0A0A0 }, true);
        let (line, col) = if cd > 0.0 {
            (format!("{cd:.1}s"), 0xFFD27F)
        } else if m.energy <= 0.0 {
            ("FREE".to_string(), 0x9BE89B)
        } else {
            (format!("{:.0} PP", m.energy), if afford { 0x8FD3FF } else { 0xFF6B6B })
        };
        host.text(b, &line, [x + 5.0, y0 + 15.0], col, true);
        // The type, shortened to three letters as in Onii's mockup.
        let kind: String = m.kind.to_uppercase().chars().take(3).collect();
        let kw2 = host.text_width(&kind);
        if cw - 13.0 - host.text_width(&line) >= kw2 {
            host.text(b, &kind, [x + cw - 4.0 - kw2, y0 + 15.0], type_color(&m.kind), true);
        }
    }
    if let Some(m) = z {
        let cd = me.cooldown_now(5);
        let fill = if m.cooldown > 0.0 { 1.0 - (cd / m.cooldown).clamp(0.0, 1.0) } else { 1.0 };
        // Without a Z-Crystal the socket is empty: no fill, no rim.
        let fill = if me.zcrystal { fill } else { 0.0 };
        let ready = cd <= 0.0 && energy >= m.energy && me.zcrystal;
        let cx = x0 + total - (ch + 4.0) / 2.0;
        z_crystal(host, b, [cx, y0 + ch / 2.0], (ch + 4.0) / 2.0, ch / 2.0 + 1.0, fill, type_color(&m.kind), ready, false);
    } else if z_locked {
        let cx = x0 + total - (ch + 4.0) / 2.0;
        z_crystal(host, b, [cx, y0 + ch / 2.0], (ch + 4.0) / 2.0, ch / 2.0 + 1.0, 0.0, 0, false, true);
    }
    Some(x0 + total)
}

/// Our own Z-crystal (a faceted diamond with a Z): its type colour fills it from the bottom as the
/// cooldown runs down, and a bright rim lights when the Z-Move is ready. With no Z-Crystal held it
/// stays an empty socket.
/// Whether this species shows a locked Z socket: it has no Z-Move only because it can still evolve.
fn z_locked_for(species: &str) -> bool {
    !species.is_empty() && !onboard::progress::is_final(species) && !sig::plain_ditto(species)
}

/// `locked`: an empty socket with a padlock instead of the Z (not fully evolved yet).
#[allow(clippy::too_many_arguments)]
fn z_crystal(host: &mut dyn Host, b: &mut HudBatch, c: [f32; 2], rx: f32, ry: f32, fill: f32, rgb: u32, ready: bool, locked: bool) {
    let diamond = |host: &mut dyn Host, b: &mut HudBatch, rx: f32, ry: f32, argb: u32| {
        host.quad(b, [[c[0], c[1] - ry], [c[0] + rx, c[1]], [c[0], c[1] + ry], [c[0] - rx, c[1]]], argb);
    };
    let rim = if ready { 0xFFFF_F4C0 } else { 0xFF5A_5E70 };
    diamond(host, b, rx, ry, rim);
    let (ix, iy) = (rx - 1.5, ry - 1.5);
    diamond(host, b, ix, iy, 0xF018_1A26);
    let w = |y: f32| ix * (1.0 - (y - c[1]).abs() / iy).max(0.0);
    let level = c[1] + iy - 2.0 * iy * fill.clamp(0.0, 1.0);
    let tint = 0xFF00_0000 | rgb;
    if fill > 0.0 {
        // Bottom half first, then the top half once the fill passes the middle.
        let mut bands = vec![(c[1] + iy, level.max(c[1]))];
        if level < c[1] {
            bands.push((c[1], level));
        }
        for (bot, top) in bands {
            host.quad(b, [[c[0] - w(top), top], [c[0] + w(top), top], [c[0] + w(bot), bot], [c[0] - w(bot), bot]], tint);
        }
    }
    // A lighter facet across the upper half reads as a cut gem.
    if level < c[1] {
        host.quad(b, [[c[0] - w(level), level], [c[0] + w(level), level], [c[0] + ix * 0.4, c[1]], [c[0] - ix * 0.4, c[1]]], 0x38FF_FFFF);
    }
    if locked {
        // A padlock: a shackle (an arch of three bars) over a solid body with a keyhole.
        let grey = 0xFFB0_B4C0;
        rect(host, b, c[0] - 3.0, c[1] - 5.0, 1.0, 4.0, grey);
        rect(host, b, c[0] + 2.0, c[1] - 5.0, 1.0, 4.0, grey);
        rect(host, b, c[0] - 2.0, c[1] - 6.0, 4.0, 1.0, grey);
        rect(host, b, c[0] - 4.0, c[1] - 1.0, 8.0, 6.0, grey);
        rect(host, b, c[0] - 0.5, c[1] + 1.0, 1.0, 2.0, 0xFF18_1A26);
        return;
    }
    let t = "Z";
    let tw = host.text_width(t);
    host.text(b, t, [c[0] - tw / 2.0 + 0.5, c[1] - 3.5], if ready { 0xFFFFFF } else { 0xB0B4C0 }, true);
}
#[cfg(test)]
mod looks_tests {
    use super::*;

    #[test]
    fn only_an_unevolved_pokemon_shows_a_locked_z() {
        assert!(z_locked_for("turtwig"), "Turtwig gets its Z-Move at Torterra");
        assert!(!z_locked_for("torterra"));
        assert!(!z_locked_for("ditto") && !z_locked_for(""));
    }

    #[test]
    fn status_chips_use_the_games_short_labels() {
        let on = |list: &'static [&'static str]| move |k: &str| list.iter().any(|x| *x == k);
        assert!(status_chips(on(&[])).is_empty());
        let labels = |c: Vec<(&'static str, u32, u32)>| c.into_iter().map(|x| x.0).collect::<Vec<_>>();
        assert_eq!(labels(status_chips(on(&["brn"]))), ["BRN"]);
        assert_eq!(labels(status_chips(on(&["psn", "tox", "par"]))), ["PAR", "PSN"], "badly poisoned is PSN, once");
        assert_eq!(labels(status_chips(on(&["tox"]))), ["PSN"]);
        assert_eq!(labels(status_chips(on(&["slp", "frz", "cnf"]))), ["SLP", "FRZ", "CNF"]);
        // Nameplate-only effects don't make chips.
        assert!(status_chips(on(&["bnd", "wet", "stun", "slow", "atk"])).is_empty());
        // Opaque chips with three-letter labels.
        for (kind, label, bg, _) in STATUS_CHIPS {
            assert_eq!(bg >> 24, 0xFF, "{kind}");
            assert_eq!(label.len(), 3, "{kind}");
        }
    }

    #[test]
    fn looks_carry_an_optional_mega_form() {
        let (looks, megas) = parse_looks("Ann:2:0:charizard_megax,Bob:1:0,Cy:3:3:,Dee:2:0:../x,broken");
        assert_eq!(looks.len(), 4);
        assert_eq!(looks["Cy"], (3, 3));
        assert_eq!(megas.len(), 1);
        assert_eq!(megas["Ann"], "charizard_megax");
        assert!(parse_looks("").0.is_empty());
    }
}

#[cfg(test)]
mod review_fixes {
    #[test]
    fn server_seconds_are_finite_and_clamped() {
        for junk in ["NaN", "nan", "inf", "-inf", "x", ""] {
            assert_eq!(super::finite_secs(junk, 30.0), 0.0, "{junk}");
            let _ = std::time::Duration::from_secs_f32(super::finite_secs(junk, 30.0));
        }
        assert_eq!(super::finite_secs("99", 30.0), 30.0);
        assert_eq!(super::finite_secs("-4", 30.0), 0.0);
        assert_eq!(super::finite_secs("2.5", 5.0), 2.5);
    }
}
