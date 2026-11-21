//! Z-Moves on Z and what the featured kits do beyond their numbers (GDD 3.1, 3.8, 3.9, 8.2).
//!
//! Final-stage and single-stage species get a Z-Move in slot 5 (the Z key), with a 75 s cooldown,
//! no energy cost and a wind-up of at least 2 s:
//! - a featured kit's own;
//! - a healer's (GDD 3.1, "some Pokémon may have healing Z-Moves instead"): its own healing move
//!   grown to itself and its whole party within `Z_HEAL_BLOCKS`;
//! - anyone else's: the species' strongest damaging level-up move at x1.6 power, reshaped into
//!   one of four big areas (`Archetype`: a cone, a zone, a long spear or a far-off nuke) under its
//!   own name. `data/zmoves.json` (docs/fx/ZMOVE_DESIGN.md) says which, per type and per species.
//!
//! The bigger the area, the longer the wind-up (`Archetype::windup`): an average Pokémon caught in
//! it has just the time to run out, one in the middle of a nuke doesn't.
//!
//! A client older than the Z key (it never says `ui z`) still casts it with F when nothing on
//! screen offers F and it's in a fight; the server decides (`onboard::use_key`).
//!
//! Wire: `storm <zapdos name> <secs>` starts Thunderstorm's darkness on every client.

use super::*;

// The server half: private, never published (docs/CLIENT_SERVER_SPLIT.md).
#[cfg(feature = "capipimon_server")]
include!("server/ultimate.rs");

/// The second Quick Attack is free this long after the first (GDD 3.9).
pub const SECOND_TICKS: u64 = 30;

/// Passive damage taken (GDD 3.9): Marshtomp -15%, Swampert -20%.
pub fn taken(species: &str) -> f32 {
    match species {
        "marshtomp" => 0.85,
        "swampert" => 0.8,
        _ => 1.0,
    }
}

/// A move that would end the caster or does nothing in real time isn't a Z-Move.
const NOT_ULTIMATES: &[&str] = &["selfdestruct", "explosion", "mistyexplosion", "finalgambit", "memento", "healingwish", "lunardance", "mindblown", "steelbeam"];

/// Moves with a behaviour of their own when they fire (several swipes, a swap, a delay): it would
/// override the Z-Move's area.
pub const OWN_BEHAVIOUR: &[&str] = &["thrash", "petaldance", "flipturn", "voltswitch", "dragontail", "roar", "whirlwind", "extrasensory", "futuresight", "fakeout", "rapidspin"];

/// The area a damaging Z-Move covers.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Archetype {
    /// A wide fan out of the caster.
    Cone,
    /// A big circle round the caster.
    Zone,
    /// A long, narrow beam.
    Spear,
    /// A huge circle on a far-off aimed point.
    Nuke,
}

/// How fast an average Pokémon runs (blocks/s), and how long it takes to notice a Z-Move.
const RUN: f32 = 5.6;
const NOTICE: f32 = 0.6;
/// The share of the way out from the worst spot that the wind-up leaves time for.
const ESCAPE: f32 = 0.8;

pub const CONE_RANGE: f32 = 24.0;
pub const CONE_DEGREES: f32 = 90.0;
pub const ZONE_RADIUS: f32 = 12.0;
pub const SPEAR_RANGE: f32 = 44.0;
pub const SPEAR_RADIUS: f32 = 2.5;
pub const NUKE_RANGE: f32 = 36.0;
pub const NUKE_RADIUS: f32 = 16.0;

impl Archetype {
    /// The farthest anyone inside has to run to get out (blocks).
    pub fn way_out(self) -> f32 {
        match self {
            // The spot on the centre line as far from the sides as from the far end.
            Archetype::Cone => {
                let s = (CONE_DEGREES / 2.0).to_radians().sin();
                CONE_RANGE * s / (1.0 + s)
            }
            Archetype::Zone => ZONE_RADIUS,
            Archetype::Spear => SPEAR_RADIUS,
            Archetype::Nuke => NUKE_RADIUS,
        }
    }

    /// The wind-up (s): time to notice it, then to run most of the way out. Never under 2 s.
    pub fn windup(self) -> f32 {
        (NOTICE + ESCAPE * self.way_out() / RUN).max(2.0)
    }

    /// Reshapes `m` into this area.
    fn shape(self, m: &mut Move) {
        let (form, shape, range, width, cone) = match self {
            Archetype::Cone => ("cone", Shape::Beam, CONE_RANGE, 2.0, CONE_DEGREES),
            Archetype::Zone => ("burst", Shape::Burst, 1.0, ZONE_RADIUS, 0.0),
            Archetype::Spear => ("beam", Shape::Beam, SPEAR_RANGE, SPEAR_RADIUS * 2.0, 0.0),
            // A ground strike's radius is 3/4 of its width.
            Archetype::Nuke => ("ground", Shape::Beam, NUKE_RANGE, NUKE_RADIUS / 0.75, 0.0),
        };
        m.form = Some(form.into());
        m.shape = shape;
        m.range = range;
        m.width = width;
        m.cone = cone;
        m.tele = Some(self.windup());
    }
}

/// One designed Z-Move (`data/zmoves.json`).
#[derive(serde::Deserialize)]
struct Design {
    name: String,
    archetype: Archetype,
    /// Per species: the type it is, when not its base move's.
    #[serde(default, rename = "type")]
    kind: Option<String>,
    /// Per species: the move it grows from, when not the strongest.
    #[serde(default)]
    from: Option<String>,
}

#[derive(serde::Deserialize)]
struct Designs {
    types: HashMap<String, Design>,
    species: HashMap<String, Design>,
}

static DESIGNS: LazyLock<Designs> = LazyLock::new(|| serde_json::from_str(include_str!("../data/zmoves.json")).expect("data/zmoves.json"));

/// The area this species' Z-Move covers (none: a featured kit's own, or a healing one).
pub fn archetype(species: &str) -> Option<Archetype> {
    let k = all_kits().by_species.get(species)?;
    if k.ultimate.is_some() || healing(k).is_some() {
        return None;
    }
    let m = of(species)?;
    DESIGNS.species.get(species).or_else(|| DESIGNS.types.get(&m.kind)).map(|d| d.archetype)
}

/// A healing Z-Move reaches the caster's party this far round it (blocks).
pub const Z_HEAL_BLOCKS: f32 = 10.0;

/// A healer's Z-Move: its own healing move, for itself and its party round it.
fn healing(k: &Kit) -> Option<Move> {
    let role = support::role(&k.species).filter(|r| r.role == "healer")?;
    let mut m = support::by_id(&role.mv)?.clone();
    let mut s = m.support.take()?;
    s.target = "allies".into();
    #[cfg(feature = "capipimon_server")]
    z_heal(&mut s);
    m.support = Some(s);
    m.shape = Shape::Burst;
    m.form = Some("burst".into());
    m.range = 1.0;
    m.width = Z_HEAL_BLOCKS;
    m.cooldown = 75.0;
    m.energy = 0.0;
    Some(m)
}

/// Whether this species' Z-Move heals instead of hitting.
pub fn heals(species: &str) -> bool {
    of(species).is_some_and(|m| m.power <= 0.0 && m.support.as_ref().is_some_and(|s| s.target == "allies"))
}

fn generated(k: &Kit) -> Option<Move> {
    if !onboard::progress::is_final(&k.species) || sig::plain_ditto(&k.species) {
        return None;
    }
    if let Some(m) = healing(k) {
        return Some(m);
    }
    let ids = onboard::progress::learnset(&k.species);
    let fits = |m: &&Move| m.power > 0.0 && !m.filler && !m.id.starts_with("sig:") && !NOT_ULTIMATES.contains(&m.id.as_str()) && !OWN_BEHAVIOUR.contains(&m.id.as_str());
    let own = DESIGNS.species.get(&k.species);
    // The designed base move when the species really learns it, else its strongest.
    let pool = || ids.iter().filter_map(|id| learn_move(id)).chain(k.moves.iter()).filter(fits);
    let named = own.and_then(|d| d.from.as_deref()).and_then(|id| pool().find(|m| m.id == id));
    let best = match named {
        Some(m) => m,
        None => pool().max_by(|a, b| a.power.total_cmp(&b.power))?,
    };
    let mut m = best.clone();
    m.power *= 1.6;
    m.cooldown = 75.0;
    m.energy = 0.0;
    m.charges = 0;
    if let Some(kind) = own.and_then(|d| d.kind.clone()) {
        m.kind = kind;
    }
    if let Some(d) = own.or_else(|| DESIGNS.types.get(&m.kind)) {
        m.name = d.name.clone();
        d.archetype.shape(&mut m);
    }
    Some(m)
}

static ULTIMATES: LazyLock<HashMap<String, Move>> = LazyLock::new(|| {
    all_kits().by_species.values().filter_map(|k| k.ultimate.clone().or_else(|| generated(k)).map(|m| (k.species.clone(), m))).collect()
});

/// This species' Z-Move, if it has one.
pub fn of(species: &str) -> Option<&'static Move> {
    ULTIMATES.get(species)
}

