//! Flight (Onii, 2026-11-16): every winged Pokémon flies the vanilla elytra way.
//!
//! - Takeoff costs no PP: a quick double tap of Space asks the server (`fly`). It puts a glider
//!   (a real elytra, unseen under the Pokémon's model) in the chest slot and answers `flyup`; the
//!   client then launches itself about 27 blocks straight up (`push_player`, the same push an
//!   explosion gives).
//! - At the top of the climb this client presses jump for one tick, so the game starts gliding as it
//!   does for any player with an elytra who jumps in the air. Gliding, steering, diving and landing
//!   are vanilla's.
//! - While gliding, PP drains every second (server). At 0 PP the server takes the glider back and the
//!   game ends the glide; the fall that follows doesn't hurt.
//! - A dodge stays a double-tapped direction key (W, A, S, D), so Space is free for this; a Pokémon
//!   that can't fly jumps as before.
//!
//! Wire: client -> server `fly`; server -> owner `flyup`; server -> everyone `fly <name> 1|g|0`
//! (launched, gliding, landed), which picks the flying animation.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

/// The winged species (`tools/winged.py`).
static WINGED: LazyLock<HashSet<String>> = LazyLock::new(|| {
    let v: serde_json::Value = serde_json::from_str(include_str!("../data/winged.json")).unwrap_or_default();
    v["winged"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default()
});

/// Whether `species` has wings to fly with.
pub fn winged(species: &str) -> bool {
    WINGED.contains(species)
}

/// PP a second while gliding. A full bar (100) glides about 12 s, a little longer than the climb
/// and dive from a launch take, so a flight is a real choice against moves.
pub const GLIDE_PP_PER_SEC: f32 = 8.0;

/// The launch's upward push (blocks a tick): about 27 blocks of climb under vanilla gravity and drag.
pub const LAUNCH_UP: f32 = 2.4;

/// Two Space presses within this long are a takeoff.
const DOUBLE_TAP_SECS: f32 = 0.3;

/// A climb that hasn't turned over by then glides anyway.
const CLIMB_SECS: f32 = 3.0;

#[derive(Default)]
struct Fly {
    /// The last Space press, for the double tap.
    last_space: Option<Instant>,
    /// The takeoff asked for and not answered yet (no second ask meanwhile).
    asked: Option<Instant>,
    /// Climbing after a launch: when it started and the height last tick.
    climb: Option<(Instant, f64)>,
    /// Jump forced off last tick, so the next forced press is a new press.
    press_next: bool,
    /// Every player's flight as the server last said: '1' launched, 'g' gliding.
    flying: HashMap<String, char>,
}

static FLY: LazyLock<Mutex<Fly>> = LazyLock::new(Default::default);

/// A Space press by this client's Pokémon (`species`): a second one soon after asks to take off.
/// Never takes the key: the first press is a vanilla jump.
pub fn space(host: &mut dyn horizon_client::mod_api::Host, species: &str, me: Option<&str>) {
    let mut f = FLY.lock().unwrap();
    let double = f.last_space.is_some_and(|t| t.elapsed().as_secs_f32() < DOUBLE_TAP_SECS);
    f.last_space = Some(Instant::now());
    if !double || !winged(species) {
        return;
    }
    f.last_space = None;
    let busy = me.is_some_and(|n| f.flying.contains_key(n)) || f.climb.is_some() || f.asked.is_some_and(|t| t.elapsed().as_secs_f32() < 1.0);
    if busy {
        return;
    }
    f.asked = Some(Instant::now());
    drop(f);
    let _ = host.net_send_to_server(crate::play::CHANNEL, b"fly");
}

/// A server line about flight. True if it was one.
pub fn heard(host: &mut dyn horizon_client::mod_api::Host, f: &[&str]) -> bool {
    match f {
        ["flyup", ..] => {
            let mut s = FLY.lock().unwrap();
            s.asked = None;
            let y = host.player().map_or(0.0, |p| p.y);
            s.climb = Some((Instant::now(), y));
            s.press_next = false;
            drop(s);
            host.push_player([0.0, LAUNCH_UP, 0.0]);
            true
        }
        ["fly", name, state, ..] => {
            let mut s = FLY.lock().unwrap();
            match state.chars().next() {
                Some(c @ ('1' | 'g')) => {
                    s.flying.insert(name.to_string(), c);
                }
                _ => {
                    s.flying.remove(*name);
                }
            }
            true
        }
        _ => false,
    }
}

/// Each local tick, after input is read: during the climb jump stays up; once the climb turns
/// over, one press of jump, which starts the vanilla glide.
pub fn movement_input(host: &dyn horizon_client::mod_api::Host, jump: &mut bool) {
    let mut s = FLY.lock().unwrap();
    if s.press_next {
        s.press_next = false;
        *jump = true;
        return;
    }
    let Some((since, last)) = s.climb else { return };
    let y = host.player().map_or(last, |p| p.y);
    // A tick or two after the push the height may not have moved yet: only a fall turns it over.
    let over = since.elapsed().as_secs_f32() > 0.25 && y < last - 0.01;
    *jump = false;
    if over || since.elapsed().as_secs_f32() > CLIMB_SECS {
        s.climb = None;
        s.press_next = true;
    } else {
        s.climb = Some((since, y));
    }
}

/// Whether `name` is flying (launched or gliding), for the flying animation.
pub fn flying(name: &str) -> bool {
    FLY.lock().unwrap().flying.contains_key(name)
}

/// Whether `name` is gliding (the server takes PP for it).
pub fn gliding(name: &str) -> bool {
    FLY.lock().unwrap().flying.get(name) == Some(&'g')
}

/// A new world: nobody flies.
pub fn forget() {
    *FLY.lock().unwrap() = Fly::default();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn birds_dragons_bats_and_bugs_fly_others_dont() {
        for s in ["charizard", "moltres", "dragonite", "zubat", "butterfree", "pidgey", "lugia", "articuno", "zapdos"] {
            assert!(winged(s), "{s} should fly");
        }
        for s in ["pikachu", "gyarados", "doduo", "piplup", "snorlax"] {
            assert!(!winged(s), "{s} shouldn't fly");
        }
    }

    #[test]
    fn a_launch_climbs_about_27_blocks() {
        // Vanilla player motion: each tick y += v, then v = (v - 0.08) * 0.98.
        let (mut v, mut y) = (f64::from(LAUNCH_UP), 0.0);
        while v > 0.0 {
            y += v;
            v = (v - 0.08) * 0.98;
        }
        assert!((24.0..=30.0).contains(&y), "climbs {y}");
    }
}
