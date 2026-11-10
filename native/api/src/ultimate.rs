//! Ultimates on F and what the featured kits do beyond their numbers (GDD 3.8, 3.9, 8.2).
//!
//! Final-stage and single-stage species get an Ultimate in slot 5 (F): a featured kit's own, or
//! the species' strongest damaging level-up move at x1.6 power, 75 s cooldown and no energy.
//!
//! F interacts only when something on screen says so and the player hasn't been in a fight for 5 s;
//! otherwise it's the Ultimate. The server decides (`onboard::use_key`), so a client only ever
//! sends `use`.
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

/// A move that would end the caster or does nothing in real time isn't an Ultimate.
const NOT_ULTIMATES: &[&str] = &["selfdestruct", "explosion", "mistyexplosion", "finalgambit", "memento", "healingwish", "lunardance", "mindblown", "steelbeam"];

fn generated(k: &Kit) -> Option<Move> {
    if !onboard::progress::is_final(&k.species) || sig::plain_ditto(&k.species) {
        return None;
    }
    let ids = onboard::progress::learnset(&k.species);
    let learned = ids.iter().filter_map(|id| learn_move(id));
    let best = learned
        .chain(k.moves.iter())
        .filter(|m| m.power > 0.0 && !m.filler && !m.id.starts_with("sig:") && !NOT_ULTIMATES.contains(&m.id.as_str()))
        .max_by(|a, b| a.power.total_cmp(&b.power))?;
    let mut m = best.clone();
    m.power *= 1.6;
    m.cooldown = 75.0;
    m.energy = 0.0;
    Some(m)
}

static ULTIMATES: LazyLock<HashMap<String, Move>> = LazyLock::new(|| {
    all_kits().by_species.values().filter_map(|k| k.ultimate.clone().or_else(|| generated(k)).map(|m| (k.species.clone(), m))).collect()
});

/// This species' Ultimate, if it has one.
pub fn of(species: &str) -> Option<&'static Move> {
    ULTIMATES.get(species)
}

