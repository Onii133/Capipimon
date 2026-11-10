//! Support moves (GDD 3.7 archetypes 8, 9 and 12): self-buffs, debuffs and status shots at foes,
//! heals for yourself and your party, drain on damaging moves, and support specialists.
//!
//! The data comes from `tools/support_moves.py` -> `data/server/support.json` (the client reads the
//! view `data/support.json` from `tools/release/client_data.py`: no effects, no drain shares):
//! - `moves`: every support move some learnset has, in the kit move format with a `support` block
//!   (power 0: the block is the move). They join the `/capi learn` and Nest PC catalogue.
//! - `drain`: damaging moves that heal the attacker by a share of the damage dealt.
//! - `roles`: specialists, whose kit gets their best support move in slot E.
//!
//! Specialists play their role better (the "reflect it in battle" part):
//! - healer: heals x1.5, a self-heal also heals party members within 8 b for half, and an ally
//!   heal with nobody in line heals the healer for half;
//! - buffer: its buffs last 10 s instead of 6;
//! - hexer: its stat drops last 10 s instead of 6;
//! - spreader: its single-target status shots open into a 70 degree cone, 8 b long.
//!
//! Only players carry statuses and stages (status.rs): wild Pokémon aren't affected yet.

use super::*;
use serde::Deserialize;

/// What a support move does, on whom (`target`): "self", "allies" (the caster and its party within
/// the burst), "ally" (the first party member along the aim), "foe" (whoever the shot reaches).
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Support {
    pub target: String,
    /// Stat stages: [["atk", 2]].
    #[serde(default)]
    pub stats: Vec<(String, i8)>,
    /// A status (par, brn, psn, tox, slp, cnf).
    #[serde(default)]
    pub ail: Option<String>,
    /// Share of max HP healed.
    #[serde(default)]
    pub heal: f32,
    /// Clears statuses and stat drops.
    #[serde(default)]
    pub cleanse: bool,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Role {
    pub role: String,
    pub slot: usize,
    #[serde(rename = "move")]
    pub mv: String,
}

#[derive(Deserialize, Default)]
struct File {
    moves: Vec<Move>,
    #[serde(default)]
    drain: HashMap<String, f32>,
    roles: HashMap<String, Role>,
}

#[cfg(not(feature = "capipimon_server"))]
const DATA: &str = include_str!("../data/support.json");

fn file() -> &'static File {
    static F: LazyLock<File> = LazyLock::new(|| serde_json::from_str(DATA).unwrap_or_default());
    &F
}

/// Every support move, in the kit move format.
pub fn moves() -> &'static [Move] {
    &file().moves
}

pub fn by_id(id: &str) -> Option<&'static Move> {
    file().moves.iter().find(|m| m.id == id)
}

/// The specialist role of a species, if it has one.
pub fn role(species: &str) -> Option<&'static Role> {
    file().roles.get(species)
}

/// Every specialist, for the kit loader.
pub fn roles() -> &'static HashMap<String, Role> {
    &file().roles
}

/// The HUD label for a role.
pub fn role_label(role: &str) -> &'static str {
    match role {
        "healer" => "HEALER",
        "buffer" => "BUFFER",
        "hexer" => "HEXER",
        "spreader" => "SPREADER",
        _ => "SUPPORT",
    }
}

// The server half: private, never published (docs/CLIENT_SERVER_SPLIT.md).
#[cfg(feature = "capipimon_server")]
include!("server/support.rs");
