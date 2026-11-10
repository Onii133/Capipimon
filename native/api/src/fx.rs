//! Move effects: glowing beams, streaks, shells and hit flashes, drawn in one full-screen pass
//! after the world (`RenderHooks::world_drawn`), plus camera shake and coloured light.
//!
//! The server says what happened (`fx`, `hit` and `guard` messages); each client draws it
//! locally. A beam ends where the server's line of sight stopped, so every client agrees.
//!
//! Techniques adapted from Orbital Laser (MIT, (c) 2026 Stink-O, commit 7853371): the world
//! position rebuilt from the depth buffer, and beams drawn as the distance from each pixel's
//! view ray to the beam's axis. No code is copied; see `licences/orbital-laser.txt` in the
//! content mod.
//!
//! Safety: everything the GPU is given is checked inside a validation error scope. If the pass
//! is rejected (an unusual frame format, a driver that dislikes the shader), it's logged once and
//! switched off for the session; the game keeps running and the moves' particles still show.

use glam::{DMat3, DVec3, Mat4, Vec3};
use horizon_client::mod_api::{
    CameraShakeEvent, Host, NativeColoredLight, RenderHooks, RenderNeedsEvent, VisualsEvent, WorldDrawnEvent, wgpu,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

/// At most this many effects are drawn in a frame (the nearest win).
const MAX_FX: usize = 16;
/// Effects further than this from the camera aren't drawn (blocks).
const CULL: f64 = 48.0;
/// How fast camera shake fades (trauma per second).
const SHAKE_DECAY: f32 = 1.6;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shape {
    Beam = 0,
    Dash = 1,
    Burst = 2,
    Hit = 3,
    Guard = 4,
    Coat = 5,
    /// A forward cone (spec common moves: Swift, Hyper Voice, Blizzard, ...).
    Cone = 6,
    /// A ground strike on a point (spec: Psychic, Icicle Crash): a ring that crushes inward.
    Ground = 7,
    /// A wind-up gathering on the caster (spec beat 1).
    Charge = 8,
    /// A wind-up's ground warning in the hitbox's shape (flags: 1 line, 2 cone, 3 circle).
    Warn = 9,
    /// A signature move's lingering area on the ground (a == b: a disc of radius r; else a band
    /// from a to b, r wide), drawn in its type's ground texture.
    Zone = 10,
    /// A status around a body (flags: 1 stun, 2 slow, 3 ward, 4 empower, 5 charged, 6 Ditto's morph goo).
    Aura = 11,
}

#[derive(Clone, Debug)]
struct Effect {
    shape: Shape,
    a: DVec3,
    b: DVec3,
    radius: f32,
    rgb: Vec3,
    seed: u32,
    start: Instant,
    secs: f32,
    /// Whose move it is: a coat follows this player's body.
    caster: String,
    /// Brightness (1 for a move; a dodge roll and the guard bubble are fainter).
    strength: f32,
    /// The type preset (`type_index`) and the move's look flags (`move_flags`), for the shader.
    style: u32,
    flags: u32,
    /// Signature moves: accent colour and `motif + 32 * grand` (`sig::look`); zeros otherwise.
    look: [f32; 4],
}

impl Effect {
    fn age(&self) -> f32 {
        self.start.elapsed().as_secs_f32() / self.secs.max(0.01)
    }
}

#[derive(Default)]
struct Fx {
    effects: Vec<Effect>,
    /// Players guarding now, by name.
    guarding: Vec<String>,
    /// Feet and model height of the players a live effect follows (a beam, burst or coat
    /// caster, or a guard), refreshed each frame.
    bodies: HashMap<String, (DVec3, f64)>,
    /// This client's player name.
    me: Option<String>,
    /// Camera shake: trauma (0-1) as of `shake_at`.
    trauma: f32,
    shake_at: Option<Instant>,
    /// Low FX: no screen pass and no coloured light (particles and shake stay).
    low: bool,
    frames: u32,
    /// This client's own hits waiting for their sound (type, where), and when it last hit.
    hit_sounds: Vec<(String, DVec3)>,
    /// Sounds for everyone: (sound, where or at whose body, volume, pitch).
    sounds: Vec<(&'static str, Result<DVec3, String>, f32, f32)>,
    hit_at: Option<Instant>,
    /// Each caster's last move's look flags and when, so its hits (which name only the type) can
    /// take the move's shape: a bite's fangs, a punch's ring, a slice's arc.
    last_flags: HashMap<String, (u32, Instant)>,
    /// `/capifx demo`: (what, page of six types, last spawned), replayed every 1.5 s.
    demo: Option<(String, usize, Option<Instant>)>,
}

const DEMO_TYPES: [&str; 18] = [
    "normal", "fire", "water", "electric", "grass", "ice", "fighting", "poison", "ground", "flying", "psychic", "bug",
    "rock", "ghost", "dragon", "dark", "steel", "fairy",
];

/// The effects of one `/capifx demo` round: six types side by side, south (+z) of the eye, for
/// looking at every type's look in game without casting (stand still, face south).
fn demo_effects(what: &str, page: usize, eye: DVec3, now: Instant) -> Vec<Effect> {
    let mut out = Vec::new();
    for (i, kind) in DEMO_TYPES.iter().skip(page * 6).take(6).enumerate() {
        // Facing south, +x is on the left: negate so the types read left to right in order.
        let x = -(i as f64 - 2.5) * if what == "cone" { 3.6 } else { 2.4 };
        let base = Effect {
            shape: Shape::Hit,
            a: eye,
            b: eye,
            radius: 0.6,
            rgb: type_rgb(kind),
            seed: 17 + i as u32,
            start: now,
            secs: 0.5,
            caster: String::new(),
            strength: 1.0,
            style: type_index(kind),
            flags: 0,
            look: [0.0; 4],
        };
        let at = |dx: f64, dy: f64, dz: f64| eye + DVec3::new(dx, dy, dz);
        out.push(match what {
            "hit" => Effect { a: at(x, -0.7, 7.0), b: at(x, -0.7, 7.0), radius: 0.8, secs: 0.4, ..base },
            "orb" => Effect { shape: Shape::Beam, a: at(x, -0.5, 2.0), b: at(x, -0.8, 14.0), secs: 2.0, look: [0.0, 0.0, 0.0, 64.0], ..base },
            "orbside" => Effect { shape: Shape::Beam, a: at(-8.0, -0.6 + i as f64 * 0.5 - 1.0, 9.0 + i as f64), b: at(8.0, -0.6 + i as f64 * 0.5 - 1.0, 9.0 + i as f64), secs: 1.6, look: [0.0, 0.0, 0.0, 64.0], ..base },
            "melee" => Effect { shape: Shape::Beam, a: at(x, -0.7, 4.0), b: at(x, -0.8, 6.5), secs: 0.6, look: [0.0, 0.0, 0.0, 128.0], ..base },
            "beam" => Effect { shape: Shape::Beam, a: at(x, -0.6, 2.0), b: at(x, -0.8, 14.0), secs: 0.8, look: [0.0, 0.0, 0.0, 192.0], ..base },
            "lunge" => Effect { shape: Shape::Dash, a: at(x + 1.0, -0.8, 4.0), b: at(x - 1.0, -0.8, 10.0), radius: 0.4, secs: 0.5, ..base },
            "cone" => Effect { shape: Shape::Cone, a: at(x, -0.6, 3.0), b: at(x, -0.8, 10.0), radius: 1.6, secs: 0.6, ..base },
            "strike" => Effect { shape: Shape::Ground, a: at(x, -1.5, 8.0), b: at(x, -1.5, 8.0), radius: 1.2, secs: 0.6, ..base },
            "burst" => Effect { shape: Shape::Burst, a: at(x, -0.8, 8.0), b: at(x, -0.8, 8.0), radius: 1.2, secs: 0.6, ..base },
            _ => return Vec::new(),
        });
    }
    out
}

/// Whether the Low FX setting is on.
pub fn low() -> bool {
    FX.lock().unwrap().low
}

/// This client landed a hit within the last quarter second (the crosshair hit marker).
pub fn hit_marked() -> bool {
    FX.lock().unwrap().hit_at.is_some_and(|t| t.elapsed().as_secs_f32() < 0.25)
}

/// The hit sound for a move type: a vanilla sound that fits, under the hit-confirm ding.
fn hit_sound(kind: &str) -> &'static str {
    match kind {
        "fire" => "minecraft:item.firecharge.use",
        "water" => "minecraft:entity.generic.splash",
        "electric" => "minecraft:entity.firework_rocket.twinkle",
        "ice" => "minecraft:block.glass.break",
        "grass" | "bug" => "minecraft:block.grass.break",
        "rock" | "ground" | "steel" => "minecraft:block.stone.break",
        "ghost" | "dark" | "psychic" => "minecraft:entity.illusioner.cast_spell",
        "poison" => "minecraft:block.slime_block.break",
        "dragon" => "minecraft:entity.ender_dragon.hurt",
        "fairy" => "minecraft:block.amethyst_block.chime",
        "flying" => "minecraft:entity.phantom.flap",
        _ => "minecraft:entity.player.attack.strong",
    }
}

static FX: LazyLock<Mutex<Fx>> = LazyLock::new(Default::default);
/// The connection ended: drop live effects, guards and shake (the Low FX choice stays).
pub fn forget() {
    let mut f = FX.lock().unwrap();
    let low = f.low;
    *f = Fx { low, ..Default::default() };
}

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

/// The shader's type preset number (move effects spec, "Type signatures").
fn type_index(kind: &str) -> u32 {
    const TYPES: [&str; 18] = [
        "normal", "fire", "water", "electric", "grass", "ice", "fighting", "poison", "ground", "flying", "psychic", "bug",
        "rock", "ghost", "dragon", "dark", "steel", "fairy",
    ];
    TYPES.iter().position(|t| *t == kind).unwrap_or(0) as u32
}

/// Showdown flags that change a move's look (spec "Flag modifiers"), from `tools/move_flags.py`:
/// bite 1, punch 2, slicing 4, pulse 8, bullet 16, wind 32, sound 64, dance 128, powder 256,
/// contact + recoil 512, multi-hit 1024.
fn move_flags(id: &str) -> u32 {
    static TABLE: LazyLock<HashMap<String, u32>> =
        LazyLock::new(|| serde_json::from_str(include_str!("../data/move_flags.json")).unwrap_or_default());
    TABLE.get(id).copied().unwrap_or(0)
}

/// The look archetype of a kit move from the move effects spec: 1 projectile (a bolt that travels),
/// 2 melee (a short curved swipe), 3 beam (a steady ray); 0 anything else (drawn by its shape).
fn arch_index(id: &str) -> u8 {
    // Moves with a line look of their own (pulse 8, bullet 16, wind 32, sound 64) keep it.
    if move_flags(id) & (8 | 16 | 32 | 64) != 0 {
        return 0;
    }
    match crate::play::move_fx(id).map(|f| f.arch.as_str()) {
        Some("Projectile") => 1,
        Some("Melee") => 2,
        Some("Beam") => 3,
        _ => 0,
    }
}

/// A move type's colour (linear RGB).
fn type_rgb(kind: &str) -> Vec3 {
    let c = match kind {
        "electric" => [1.0, 0.85, 0.25],
        "fire" => [1.0, 0.42, 0.08],
        "water" => [0.25, 0.55, 1.0],
        "grass" => [0.35, 1.0, 0.35],
        "ice" => [0.55, 0.95, 1.0],
        "fighting" => [1.0, 0.55, 0.3],
        "poison" => [0.75, 0.3, 1.0],
        "ground" => [0.85, 0.65, 0.3],
        "flying" => [0.7, 0.8, 1.0],
        "psychic" => [1.0, 0.35, 0.7],
        "bug" => [0.65, 0.9, 0.2],
        "rock" => [0.8, 0.7, 0.45],
        "ghost" => [0.55, 0.4, 1.0],
        "dragon" => [0.45, 0.35, 1.0],
        "dark" => [0.6, 0.35, 0.55],
        "steel" => [0.75, 0.8, 0.9],
        "fairy" => [1.0, 0.6, 0.9],
        _ => [1.0, 1.0, 1.0],
    };
    Vec3::from(c)
}

fn parse_v3(s: &str) -> Option<DVec3> {
    let mut it = s.split(',').map(|p| p.trim().parse::<f64>());
    let (Some(Ok(x)), Some(Ok(y)), Some(Ok(z)), None) = (it.next(), it.next(), it.next(), it.next()) else {
        return None;
    };
    Some(DVec3::new(x, y, z))
}

/// A server message for the effects, if it is one. `me` is this client's player name.
pub fn heard(f: &[&str], me: Option<&str>) -> bool {
    let mut fx = FX.lock().unwrap();
    match f {
        // fx <move> <type> <beam|dash|burst> <a> <b> <radius> <caster> <seed> <secs>
        ["fx", id, kind, shape, a, b, radius, caster, seed, secs, ..] => {
            let (Some(a), Some(b)) = (parse_v3(a), parse_v3(b)) else { return true };
            let shape = match *shape {
                "beam" => Shape::Beam,
                "dash" => Shape::Dash,
                "burst" => Shape::Burst,
                "cone" => Shape::Cone,
                "ground" => Shape::Ground,
                "zone" => Shape::Zone,
                _ => return true,
            };
            let secs = secs.parse::<f32>().unwrap_or(0.5).clamp(0.05, 20.0);
            let e = Effect {
                shape,
                a,
                b,
                radius: radius.parse::<f32>().unwrap_or(0.4).clamp(0.05, 16.0),
                rgb: type_rgb(kind),
                seed: seed.parse().unwrap_or(0),
                start: Instant::now(),
                secs,
                caster: caster.to_string(),
                strength: 1.0,
                style: type_index(kind),
                flags: move_flags(id),
                look: crate::play::sig::look(id).unwrap_or_default(),
            };
            // The spec archetype of a kit move, so projectiles, melee swipes and beams sent as the
            // same line shape still look different (`look[3] += 64 * arch`).
            let mut e = e;
            e.look[3] += 64.0 * f32::from(arch_index(id));
            // A dodge roll is a faint pale streak, not a move.
            let e = match crate::play::sig::of(id) {
                Some(s) => Effect { flags: s.flags, ..e },
                None => e,
            };
            let e = if *id == "dodge" { Effect { rgb: Vec3::new(0.7, 0.85, 1.0), strength: 0.3, style: 0, flags: 0, ..e } } else { e };
            if *id != "dodge" {
                fx.last_flags.retain(|_, (_, at)| at.elapsed().as_secs_f32() < 3.0);
                fx.last_flags.insert(caster.to_string(), (e.flags, Instant::now()));
            }
            if *id != "dodge" && shape != Shape::Zone {
                // Spec beat 2, release: one very bright frame where it leaves, and its sound. The
                // wind-up (if any) is over.
                let at = if shape == Shape::Ground { b } else { a };
                if matches!(shape, Shape::Beam | Shape::Cone) {
                    let flash = Effect { shape: Shape::Hit, b: a, radius: 0.35, secs: 0.12, flags: 0, ..e.clone() };
                    fx.effects.push(flash);
                }
                fx.effects.retain(|x| !(x.caster == *caster && matches!(x.shape, Shape::Charge | Shape::Warn)));
                if fx.sounds.len() < 8 {
                    fx.sounds.push((hit_sound(kind), Ok(at), 0.55, 1.35));
                }
            }
            // Mechanic dressing for signatures, drawn with the existing shapes (no wire change):
            // a meteor strike gets a comet falling onto it, a blink a poof where it vanished and
            // where it reappears.
            if *id != "dodge" && shape != Shape::Zone && crate::play::sig::has_mech(id, "meteor") {
                let at = if matches!(shape, Shape::Ground | Shape::Burst) { a } else { b };
                let sky = at + DVec3::new(1.5, 14.0, 0.8);
                fx.effects.push(Effect { shape: Shape::Dash, a: sky, b: at, radius: e.radius.clamp(0.5, 1.4), secs: 0.24, strength: 1.3, ..e.clone() });
            }
            if shape == Shape::Dash && crate::play::sig::has_mech(id, "blink") {
                for p in [a, b] {
                    fx.effects.push(Effect { shape: Shape::Hit, a: p, b: p, radius: 1.0, secs: 0.35, flags: 0, ..e.clone() });
                }
            }
            // A fire dash also sets the caster alight while it lasts (Flame Charge).
            if shape == Shape::Dash && *kind == "fire" {
                let coat = Effect { shape: Shape::Coat, radius: 0.9, secs: secs * 2.0, ..e.clone() };
                fx.effects.push(coat);
            }
            fx.effects.push(e);
            if fx.effects.len() > 64 {
                fx.effects.remove(0);
            }
            true
        }
        // hit <who> <at> <type> <damage> [<attacker>]
        ["hit", who, at, kind, dmg, rest @ ..] => {
            let Some(at) = parse_v3(at) else { return true };
            let dmg = dmg.parse::<f32>().unwrap_or(0.0);
            let flags = rest
                .first()
                .and_then(|by| fx.last_flags.get(*by))
                .filter(|(_, t)| t.elapsed().as_secs_f32() < 2.0)
                .map_or(0, |(f, _)| *f);
            fx.effects.push(Effect {
                shape: Shape::Hit,
                a: at,
                b: at,
                radius: (0.45 + dmg / 40.0).min(1.2),
                rgb: type_rgb(kind),
                seed: dmg as u32,
                start: Instant::now(),
                secs: if flags & 7 != 0 { 0.4 } else { 0.3 },
                caster: who.to_string(),
                strength: 1.0,
                style: type_index(kind),
                flags,
                look: [0.0; 4],
            });
            if me.is_some() && rest.first().copied() == me && me != Some(*who) {
                fx.hit_at = Some(Instant::now());
                if fx.hit_sounds.len() < 8 {
                    fx.hit_sounds.push((kind.to_string(), at));
                }
            }
            if me == Some(*who) {
                let now = trauma_now(&fx);
                fx.trauma = (now + (dmg / 25.0).clamp(0.2, 0.6)).min(1.0);
                fx.shake_at = Some(Instant::now());
            }
            true
        }
        // charge <who> <move> <type> <secs> <warn: line|cone|circle|-> [<a> <b> <radius>]: a wind-up
        // started (spec beat 1); charge <who> -: it was interrupted. Not consumed: the client's
        // own wind-up state reads it too.
        ["charge", who, "-", ..] => {
            fx.effects.retain(|x| !(x.caster == *who && matches!(x.shape, Shape::Charge | Shape::Warn)));
            false
        }
        ["charge", who, id, kind, secs, warn, rest @ ..] => {
            let secs = secs.parse::<f32>().unwrap_or(0.3).clamp(0.05, 5.0);
            let base = Effect {
                shape: Shape::Charge,
                a: DVec3::ZERO,
                b: DVec3::ZERO,
                radius: 0.8,
                rgb: type_rgb(kind),
                seed: secs.to_bits() % 997,
                start: Instant::now(),
                secs,
                caster: who.to_string(),
                strength: 1.0,
                style: type_index(kind),
                flags: crate::play::sig::of(id).map_or_else(|| move_flags(id), |s| s.flags),
                look: crate::play::sig::look(id).unwrap_or_default(),
            };
            let kindw = match *warn {
                "line" => 1,
                "cone" => 2,
                "circle" => 3,
                _ => 0,
            };
            if let ([a, b, r, ..], true) = (rest, kindw > 0) {
                if let (Some(a), Some(b)) = (parse_v3(a), parse_v3(b)) {
                    let radius = r.parse::<f32>().unwrap_or(1.0).clamp(0.2, 16.0);
                    fx.effects.push(Effect { shape: Shape::Warn, a, b, radius, flags: kindw, ..base.clone() });
                }
            }
            fx.effects.push(base);
            if fx.sounds.len() < 8 {
                // A rising charge whine, higher for short wind-ups.
                fx.sounds.push(("minecraft:block.respawn_anchor.charge", Err(who.to_string()), 0.45, (1.6 - secs * 0.5).clamp(0.7, 1.6)));
            }
            if fx.effects.len() > 64 {
                fx.effects.remove(0);
            }
            false
        }
        // morph <who> <secs> <species>: Ditto's goo while it changes (not consumed: sig.rs reads it too).
        ["morph", who, secs, ..] => {
            let secs = secs.parse::<f32>().unwrap_or(0.6).clamp(0.1, 3.0);
            fx.effects.push(Effect {
                shape: Shape::Aura,
                a: DVec3::ZERO,
                b: DVec3::ZERO,
                radius: 1.0,
                rgb: Vec3::new(0.7, 0.35, 1.0),
                seed: 7,
                start: Instant::now(),
                secs,
                caster: who.to_string(),
                strength: 1.0,
                style: 0,
                flags: 6,
                look: [0.85, 0.6, 1.0, 22.0],
            });
            if fx.sounds.len() < 8 {
                fx.sounds.push(("minecraft:block.slime_block.place", Err(who.to_string()), 0.8, 0.8));
            }
            false
        }
        // guard <who> <1|0>
        ["guard", who, on, ..] => {
            fx.guarding.retain(|g| g != who);
            if *on == "1" {
                fx.guarding.push(who.to_string());
            }
            true
        }
        _ => false,
    }
}

fn trauma_now(fx: &Fx) -> f32 {
    match fx.shake_at {
        Some(t) => (fx.trauma - SHAKE_DECAY * t.elapsed().as_secs_f32()).max(0.0),
        None => 0.0,
    }
}

/// `/capifx dbg`: show what the effects pass draws in the action bar (testing only).
static DBG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static DBG_LINE: Mutex<String> = Mutex::new(String::new());

/// Each frame (game thread): forget finished effects, follow bodies, read the Low FX setting.
pub fn frame(host: &mut dyn Host) {
    if DBG.load(Ordering::Relaxed) {
        let line = DBG_LINE.lock().unwrap().clone();
        if !line.is_empty() {
            host.client_action_bar(horizon_client::mod_api::Text::literal(&line), false);
        }
    }
    let mut fx = FX.lock().unwrap();
    for (kind, at) in std::mem::take(&mut fx.hit_sounds) {
        let at = at.to_array();
        host.play_local_sound(hit_sound(&kind), at, 0.8, 1.0, 0);
        host.play_local_sound("minecraft:entity.arrow.hit_player", at, 0.45, 1.3, 0);
    }
    // Sounds placed in the world play now; sounds at someone's body wait for it below.
    let sounds = std::mem::take(&mut fx.sounds);
    let mut at_body = Vec::new();
    for (sound, at, vol, pitch) in sounds {
        match at {
            Ok(p) => {
                host.play_local_sound(sound, p.to_array(), vol, pitch, 0);
            }
            Err(who) => at_body.push((sound, who, vol, pitch)),
        }
    }
    fx.effects.retain(|e| e.age() < 1.0);
    if let Some((what, page, last)) = fx.demo.clone() {
        if last.map_or(true, |t| t.elapsed().as_secs_f32() > 1.5) {
            if let Some(eye) = host.client_state().and_then(|s| s.eye_position) {
                let now = Instant::now();
                let round = demo_effects(&what, page, DVec3::from(eye), now);
                fx.effects.extend(round);
                fx.demo = Some((what, page, Some(now)));
            }
        }
    }
    fx.frames = fx.frames.wrapping_add(1);
    if fx.frames % 60 == 1 {
        fx.low = host.setting(crate::MOD_ID, "fx").as_deref() == Some("low");
    }
    // A beam, a burst and a fire coat stay with their caster (who keeps moving after the server
    // placed them), and so does a guard shell.
    let follow: Vec<String> = fx
        .effects
        .iter()
        .filter(|e| matches!(e.shape, Shape::Coat | Shape::Beam | Shape::Burst | Shape::Charge | Shape::Aura))
        .map(|e| e.caster.clone())
        .chain(fx.guarding.iter().cloned())
        .chain(crate::play::sig::auras().into_iter().map(|(n, ..)| n))
        .collect();
    fx.bodies.clear();
    if follow.is_empty() {
        return;
    }
    drop(fx);
    let (me, species) = {
        let c = crate::play::CLIENT.lock().unwrap();
        let species: HashMap<String, String> =
            follow.iter().filter_map(|n| c.species_by_name.get(n).map(|s| (n.clone(), s.clone()))).collect();
        (c.my_name.clone(), species)
    };
    // The model's height (players keep the vanilla 1.8 body, so that is no guide).
    let height = |name: &str| {
        species.get(name).and_then(|s| crate::play::kit(s)).map_or(1.8, |k| f64::from(k.body))
    };
    let state = host.client_state();
    let partial = state.as_ref().map_or(1.0, |s| s.world_partial_tick.clamp(0.0, 1.0));
    let mut bodies = HashMap::new();
    for e in host.client_entities() {
        let Some(name) = e.player_name else { continue };
        if !follow.contains(&name) {
            continue;
        }
        let p = DVec3::from(e.previous_position).lerp(DVec3::from(e.position), partial);
        let h = height(&name);
        bodies.insert(name, (p, h));
    }
    // This client's own player isn't among the entities: its eye (the camera's interpolated feet
    // replace this in third person, see `snapshot`).
    if let (Some(me), Some(eye)) = (me.as_ref(), state.as_ref().and_then(|s| s.eye_position)) {
        if follow.contains(me) {
            bodies.insert(me.clone(), (DVec3::from(eye) - DVec3::Y * 1.62, height(me)));
        }
    }
    for (sound, who, vol, pitch) in at_body {
        if let Some((feet, h)) = bodies.get(&who) {
            host.play_local_sound(sound, (*feet + DVec3::Y * h * 0.5).to_array(), vol, pitch, 0);
        }
    }
    let mut fx = FX.lock().unwrap();
    fx.bodies = bodies;
    fx.me = me;
}

/// `/capifx low` or `/capifx high` (client only): the Low FX toggle, kept between sessions.
pub fn command(host: &mut dyn Host, command: &str) -> bool {
    let mut words = command.trim_start_matches('/').split_whitespace();
    if words.next() != Some("capifx") {
        return false;
    }
    if command.contains("dbg") {
        let on = !DBG.load(Ordering::Relaxed);
        DBG.store(on, Ordering::Relaxed);
        return true;
    }
    // `/capifx demo <hit|orb|orbside|melee|beam|lunge|cone|strike|burst> [page 0-2]` or `demo off`:
    // six types' looks replayed in front of you (face south), for checking effects in game.
    if command.split_whitespace().nth(1) == Some("demo") {
        let mut w = command.split_whitespace().skip(2);
        let what = w.next().unwrap_or("off").to_string();
        let page = w.next().and_then(|p| p.parse::<usize>().ok()).unwrap_or(0).min(2);
        let mut fx = FX.lock().unwrap();
        let on = demo_effects(&what, page, DVec3::ZERO, Instant::now()).len() == 6;
        fx.demo = on.then(|| (what.clone(), page, None));
        drop(fx);
        let msg = if on { format!("Capipimon FX demo: {what}, types {}-{}", page * 6 + 1, page * 6 + 6) } else { "Capipimon FX demo off".to_string() };
        host.client_action_bar(horizon_client::mod_api::Text::literal(&msg), false);
        return true;
    }
    let low = match words.next() {
        Some("low") => true,
        Some("high") => false,
        _ => !FX.lock().unwrap().low,
    };
    host.set_setting(crate::MOD_ID, "fx", if low { "low" } else { "high" });
    FX.lock().unwrap().low = low;
    host.client_action_bar(
        horizon_client::mod_api::Text::literal(if low { "Capipimon FX: low (particles only)" } else { "Capipimon FX: high" }),
        false,
    );
    true
}

/// Camera shake when this player is hit.
pub fn camera_shake(event: &mut CameraShakeEvent<'_>) {
    let t = trauma_now(&FX.lock().unwrap());
    if t <= 0.0 {
        return;
    }
    let k = t * t;
    let s = EPOCH.elapsed().as_secs_f32() * 22.0;
    let n = |o: f32| (s + o).sin() * 0.6 + (s * 2.3 + o * 1.7).sin() * 0.4;
    event.shake.pitch += k * 3.5 * n(0.0);
    event.shake.yaw += k * 3.5 * n(11.0);
    event.shake.roll += k * 2.5 * n(23.0);
}

fn rgb_u32(c: Vec3) -> u32 {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0) as u32;
    (b(c.x) << 16) | (b(c.y) << 8) | b(c.z)
}

/// Coloured light from live effects, so a Thunderbolt lights the wall it hits.
pub fn visuals(event: &mut VisualsEvent<'_>) {
    let fx = FX.lock().unwrap();
    if fx.low {
        return;
    }
    for e in fx.effects.iter().filter(|e| !matches!(e.shape, Shape::Coat | Shape::Guard | Shape::Warn | Shape::Aura)).take(8) {
        let fade = (1.0 - e.age()).clamp(0.0, 1.0);
        let at = match e.shape {
            Shape::Beam | Shape::Dash | Shape::Cone => e.b,
            _ => e.a,
        };
        // Soft: at full strength the light washed out everything around a hit.
        let (radius, intensity) = match e.shape {
            Shape::Beam => (5.0, 0.35),
            Shape::Burst => (e.radius + 2.0, 0.3),
            Shape::Zone => (e.radius + 1.5, 0.2),
            _ => (3.0, 0.25),
        };
        if e.strength < 0.5 {
            continue;
        }
        event.lights.push(NativeColoredLight { at: at.to_array(), rgb: rgb_u32(e.rgb), radius, intensity: intensity * fade });
    }
}

/// One effect as the shader reads it (4 vec4s).
fn pack(e: &Effect, a: Vec3, b: Vec3, age: f32, out: &mut Vec<f32>) {
    out.extend_from_slice(&[a.x, a.y, a.z, e.radius]);
    out.extend_from_slice(&[b.x, b.y, b.z, e.shape as u32 as f32]);
    out.extend_from_slice(&[e.rgb.x, e.rgb.y, e.rgb.z, e.strength]);
    out.extend_from_slice(&[age.clamp(0.0, 1.0), (e.seed % 997) as f32, (e.style + 32 * e.flags) as f32, e.secs]);
    out.extend_from_slice(&e.look);
}

/// Floats per effect in the uniform buffer (5 vec4s).
const FX_FLOATS: usize = 20;

/// What to draw this frame, camera-relative, nearest first; empty when nothing is live.
fn snapshot(camera: DVec3) -> Vec<f32> {
    let fx = FX.lock().unwrap();
    if fx.low {
        return Vec::new();
    }
    let mut live: Vec<(f64, Effect)> = Vec::new();
    let mut add = |e: Effect| {
        // Distance from the camera to the effect's segment.
        let ab = e.b - e.a;
        let s = if ab.length_squared() > 1e-9 { ((camera - e.a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0) } else { 0.0 };
        let d = (e.a + ab * s).distance(camera) - f64::from(e.radius);
        if d < CULL {
            live.push((d, e));
        }
    };
    // Where each followed player is now; this client's own feet come from this frame's camera.
    let local = crate::play::local_feet().map(DVec3::from);
    let body = |name: &str| -> Option<(DVec3, f64)> {
        let (feet, h) = *fx.bodies.get(name)?;
        let feet = if fx.me.as_deref() == Some(name) { local.unwrap_or(feet) } else { feet };
        Some((feet, h))
    };
    for e in &fx.effects {
        if e.age() >= 1.0 {
            continue;
        }
        match (e.shape, body(&e.caster)) {
            (Shape::Coat, Some((feet, h))) => {
                let c = feet + DVec3::Y * h * 0.5;
                add(Effect { a: c, b: c, radius: (h * 0.55).clamp(0.45, 3.0) as f32, ..e.clone() });
            }
            (Shape::Coat, None) => {}
            // The beam keeps its aim and length, from the caster's chest where it is now.
            (Shape::Beam, Some((feet, h))) => {
                let a = feet + DVec3::Y * h * crate::play::BEAM_FROM;
                add(Effect { a, b: a + (e.b - e.a), ..e.clone() });
            }
            (Shape::Charge, Some((feet, h))) => {
                let c = feet + DVec3::Y * h * 0.55;
                add(Effect { a: c, b: c, radius: (h * 0.6).clamp(0.5, 3.0) as f32, ..e.clone() });
            }
            (Shape::Charge, None) => {}
            (Shape::Aura, Some((feet, h))) => {
                let c = feet + DVec3::Y * h * 0.5;
                add(Effect { a: c, b: c, radius: (h * 0.6 + 0.3).clamp(0.6, 3.5) as f32, ..e.clone() });
            }
            (Shape::Aura, None) => {}
            (Shape::Burst, Some((feet, h))) => {
                let c = feet + DVec3::Y * h * 0.5;
                add(Effect { a: c, b: c, ..e.clone() });
            }
            _ => add(e.clone()),
        }
    }
    for g in &fx.guarding {
        let Some((feet, h)) = body(g) else { continue };
        let at = feet + DVec3::Y * h * 0.5;
        add(Effect {
            shape: Shape::Guard,
            a: at,
            b: at,
            radius: (h * 0.6 + 0.4).clamp(0.7, 4.0) as f32,
            rgb: Vec3::new(0.55, 0.8, 1.0),
            seed: 0,
            start: Instant::now(),
            secs: 1.0,
            caster: g.clone(),
            strength: 0.5,
            style: 0,
            flags: 0,
            look: [0.0; 4],
        });
    }
    // Statuses from signature moves (stun, slow, ward, empower, charged), on whoever has them.
    for (name, kind, left) in crate::play::sig::auras() {
        let Some((feet, h)) = body(&name) else { continue };
        let at = feet + DVec3::Y * h * 0.5;
        let rgb = match kind {
            1 => Vec3::new(1.0, 0.85, 0.2),
            2 => Vec3::new(0.35, 0.6, 1.0),
            3 => Vec3::new(0.35, 0.95, 0.95),
            4 => Vec3::new(1.0, 0.45, 0.15),
            _ => Vec3::new(1.0, 0.9, 0.3),
        };
        add(Effect {
            shape: Shape::Aura,
            a: at,
            b: at,
            radius: (h * 0.6 + 0.3).clamp(0.6, 3.5) as f32,
            rgb,
            seed: kind,
            start: Instant::now(),
            secs: 1.0,
            caster: name,
            strength: if left < 0.4 { left / 0.4 } else { 1.0 } * 0.8,
            style: 0,
            flags: kind,
            look: [0.0; 4],
        });
    }
    if DBG.load(Ordering::Relaxed) {
        let f = |v: DVec3| format!("{:.1},{:.1},{:.1}", v.x, v.y, v.z);
        let mut line = format!("cam {} me {:?} local {:?} n {} live {}", f(camera), fx.me, local.map(f), fx.effects.len(), live.len());
        for e in &fx.effects {
            line += &format!(" | {:?} {} body {:?} a {} ab {:.1} age {:.2}", e.shape, e.caster, body(&e.caster).map(|b| f(b.0)), f(e.a), (e.b - e.a).length(), e.age());
        }
        for (d, e) in &live {
            line += &format!(" || {:?} d {:.1} a {}", e.shape, d, f(e.a));
        }
        *DBG_LINE.lock().unwrap() = line;
    }
    drop(fx);
    if live.is_empty() {
        return Vec::new();
    }
    live.sort_by(|x, y| x.0.total_cmp(&y.0));
    let mut out = Vec::with_capacity(MAX_FX * FX_FLOATS);
    for (_, e) in live.iter().take(MAX_FX) {
        let a = (e.a - camera).as_vec3();
        let b = (e.b - camera).as_vec3();
        // A guard and a status aura are steady (statuses are rebuilt every frame).
        let age = if e.shape == Shape::Guard || (e.shape == Shape::Aura && e.flags != 6) { 0.5 } else { e.age() };
        pack(e, a, b, age, &mut out);
    }
    out
}

const SHADER: &str = r#"
struct Fx { a: vec4<f32>, b: vec4<f32>, color: vec4<f32>, p: vec4<f32>, m: vec4<f32> };
struct U { inv: mat4x4<f32>, screen: vec4<f32>, info: vec4<f32>, eye: vec4<f32>, fx: array<Fx, 16> };
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var depth_tex: texture_depth_2d;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    return vec4<f32>(x * 2.0 - 1.0, y * 2.0 - 1.0, 0.0, 1.0);
}

fn unproject(ndc: vec3<f32>) -> vec3<f32> {
    let p = u.inv * vec4<f32>(ndc, 1.0);
    return p.xyz / p.w;
}

fn sq(x: f32) -> f32 { return x * x; }
fn hash(n: f32) -> f32 { return fract(sin(n) * 43758.5453); }
fn noise1(x: f32) -> f32 {
    let i = floor(x);
    let f = fract(x);
    let s = f * f * (3.0 - 2.0 * f);
    return mix(hash(i), hash(i + 1.0), s) * 2.0 - 1.0;
}

// Closest approach of the view ray (origin 0, unit rd, t in [0, tmax]) to the segment a..b.
// Returns (ray point, segment param s).
fn ray_seg(rd: vec3<f32>, tmax: f32, a: vec3<f32>, b: vec3<f32>) -> vec4<f32> {
    let ab = b - a;
    let c = max(dot(ab, ab), 1e-6);
    let bb = dot(rd, ab);
    var s = 0.0;
    let den = c - bb * bb;
    if (den > 1e-5) {
        s = clamp((dot(ab, -a) - bb * dot(rd, -a)) / den, 0.0, 1.0);
    }
    var t = clamp(dot(a + ab * s, rd), 0.0, tmax);
    s = clamp(dot(rd * t - a, ab) / c, 0.0, 1.0);
    t = clamp(dot(a + ab * s, rd), 0.0, tmax);
    return vec4<f32>(rd * t, s);
}

// A sphere at c: distance from its centre to the visible part of the ray, and whether its
// front surface (radius r) is in front of the scene.
fn ray_ball(rd: vec3<f32>, tmax: f32, c: vec3<f32>, r: f32) -> vec2<f32> {
    let tc = dot(c, rd);
    let h = length(rd * clamp(tc, 0.0, tmax) - c);
    let front = tc - sqrt(max(r * r - h * h, 0.0));
    // Soft where the scene cuts in: a hard step left a glow's halo sliced off by the ground.
    let vis = clamp((tmax - front) / max(r * 0.7, 0.05) + 0.5, 0.0, 1.0);
    return vec2<f32>(h, select(0.0, vis, tc > -r));
}

fn ln(d: f32, w: f32) -> f32 { return exp(-sq(d / w)); }
fn rot2(p: vec2<f32>, a: f32) -> vec2<f32> {
    let c = cos(a);
    let s = sin(a);
    return vec2<f32>(c * p.x - s * p.y, s * p.x + c * p.y);
}

// A signature move's motif (sig.rs MOTIFS order), drawn as light in a disc where |uv| = 1 is the
// edge. Lines are thin and bright; the accent colour (f.m.rgb) paints it.
fn motif(k: i32, uv0: vec2<f32>, t: f32, seed: f32) -> f32 {
    let uv = uv0;
    let rho = length(uv);
    // Every motif lives in the unit disc: skip the rest (most pixels of a stamped line) outside it.
    if (rho > 1.3) { return 0.0; }
    let ang = atan2(uv.y, uv.x);
    let tau = 6.28318;
    var v = 0.0;
    switch k {
        case 1: { // runes: a turning ring of glyph ticks between two circles
            let a2 = ang + t * 0.8;
            let cell = floor((a2 / tau + 1.0) * 10.0);
            let g = step(0.35, hash(cell + seed)) * step(0.2, fract((a2 / tau + 1.0) * 10.0)) * step(fract((a2 / tau + 1.0) * 10.0), 0.8);
            v = ln(rho - 0.88, 0.025) + ln(rho - 0.55, 0.025) + g * ln(rho - 0.715, 0.07) * 0.9;
        }
        case 2: { // star
            let p = rot2(uv, t * 0.6);
            let a = atan2(p.y, p.x);
            let kk = 0.5 + 0.5 * cos(a * 5.0);
            let d = rho - 0.85 * (0.42 + 0.58 * pow(kk, 3.0));
            v = ln(d, 0.03) + 0.15 * step(d, 0.0);
        }
        case 3: { // crescent
            let d = max(rho - 0.85, -(length(uv - vec2<f32>(0.32, 0.18)) - 0.7));
            v = ln(d, 0.03) + 0.25 * step(d, 0.0);
        }
        case 4: { // spiral
            v = pow(0.5 + 0.5 * sin(ang * 2.0 + rho * 9.0 - t * 6.0), 10.0) * (1.0 - smoothstep(0.7, 1.0, rho));
        }
        case 5: { // pillar: a hot core with a cross of light
            v = exp(-sq(rho / 0.22)) * 1.4 + (ln(uv.x, 0.04) + ln(uv.y, 0.04)) * (1.0 - rho) * 0.6;
        }
        case 6: { // petals
            let d = rho - 0.85 * abs(cos(2.5 * (ang + t * 0.4)));
            v = ln(d, 0.035) + 0.2 * step(d, 0.0);
        }
        case 7: { // shards: a crown of jagged triangles
            let f = fract((ang / tau + 1.0) * 9.0);
            let r0 = 0.45 + 0.5 * hash(floor((ang / tau + 1.0) * 9.0) + seed);
            let edge = r0 * (1.0 - abs(f - 0.5) * 2.0);
            v = ln(rho - edge, 0.03) + 0.18 * step(rho, edge);
        }
        case 8: { // smoke: soft rolling blobs
            // A ring of puffs rolling round, each with a lit rim, swelling and shrinking.
            for (var i = 0; i < 7; i = i + 1) {
                let fi = f32(i);
                let a = fi * 0.8976 + t * 0.7;
                let c = vec2<f32>(cos(a), sin(a)) * (0.5 + 0.08 * sin(t * 2.0 + fi));
                let pr = 0.2 + 0.07 * sin(t * 3.0 + fi * 2.3 + seed);
                let d = length(uv - c) - pr;
                v = max(v, ln(d, 0.035) * 0.9 + smoothstep(0.0, -0.12, d) * 0.3);
            }
        }
        case 9: { // hearts
            // iq's heart distance, beating; the inner outline echoes it.
            var p = uv * 0.72 / (1.0 + 0.08 * sin(t * 7.0)) + vec2<f32>(0.0, 0.52);
            p.x = abs(p.x);
            var d = 0.0;
            if (p.y + p.x > 1.0) {
                d = length(p - vec2<f32>(0.25, 0.75)) - 0.35355;
            } else {
                let q = p - 0.5 * max(p.x + p.y, 0.0);
                d = sqrt(min(dot(p - vec2<f32>(0.0, 1.0), p - vec2<f32>(0.0, 1.0)), dot(q, q))) * sign(p.x - p.y);
            }
            v = ln(d, 0.025) + ln(d + 0.12, 0.018) * 0.5 + 0.15 * step(d, 0.0);
        }
        case 10: { // gears: a toothed ring that turns, and a hub
            let rr = 0.72 + 0.1 * step(0.0, sin((ang + t * 0.7) * 12.0));
            v = ln(rho - rr, 0.03) + ln(rho - 0.32, 0.03) + ln(rho - 0.55, 0.02) * 0.4;
        }
        case 11: { // scales: rows of overlapping arcs
            let g = uv * 3.5;
            let c = fract(vec2<f32>(g.x + 0.5 * floor(g.y), g.y)) - vec2<f32>(0.5, 0.0);
            v = ln(length(c) - 0.5, 0.05) * step(c.y, 0.5) * (1.0 - smoothstep(0.75, 1.0, rho));
        }
        case 12: { // eye: an almond with a pupil that blinks
            let open = 0.45 * (1.0 - step(0.93, fract(t * 0.5)));
            let d = abs(uv.y) - open * (1.0 - uv.x * uv.x);
            v = ln(d, 0.03) * step(abs(uv.x), 1.0) + exp(-sq(rho / 0.17)) * 1.2 * step(0.01, open);
        }
        case 13: { // comet: a head with a tail behind it
            let hd = length(uv - vec2<f32>(0.4, 0.0));
            let tail = exp(-sq(uv.y / (0.04 + 0.2 * (0.4 - uv.x)))) * step(uv.x, 0.4) * smoothstep(-1.0, 0.4, uv.x);
            v = exp(-sq(hd / 0.18)) * 1.5 + tail * 0.7;
        }
        case 14: { // quake: radial cracks
            let w = (ang / tau + 1.0) * 6.0 + noise1(rho * 7.0 + seed) * 0.15;
            v = ln((fract(w) - 0.5) * rho * 1.05, 0.025) * step(0.12, rho) * (1.0 - smoothstep(0.85, 1.0, rho)) + ln(rho - 0.1, 0.03);
        }
        case 15: { // vortex: arms winding in
            v = pow(0.5 + 0.5 * sin(ang * 3.0 - log(rho + 0.05) * 6.0 + t * 8.0), 6.0) * (1.0 - smoothstep(0.6, 1.0, rho));
        }
        case 16: { // bubbles
            let g = uv * 3.0 + vec2<f32>(0.0, -t * 0.8);
            let c = fract(g) - 0.5;
            let id = floor(g);
            let rr = 0.12 + 0.22 * hash(id.x * 7.0 + id.y * 13.0 + seed);
            v = ln(length(c) - rr, 0.035) * (1.0 - smoothstep(0.75, 1.0, rho));
        }
        case 17: { // web: spokes and rings
            let f = fract((ang / tau + 1.0) * 8.0) - 0.5;
            v = (ln(f * rho * 0.785, 0.015) + ln(fract(rho * 4.0) - 0.5, 0.06) * 0.6) * (1.0 - smoothstep(0.9, 1.0, rho));
        }
        case 18: { // feathers: chevrons sweeping along
            v = ln(fract(uv.x * 2.5 + abs(uv.y) * 1.6 - t * 2.0) - 0.5, 0.06) * step(abs(uv.y), 0.65) * (1.0 - smoothstep(0.8, 1.0, rho));
        }
        case 19: { // echo: rings running outward
            // Sound waves: arcs running out in two opposite fans, brighter near the source.
            let w = fract(rho * 4.0 - t * 2.5);
            let fan = pow(abs(cos(ang)), 1.5);
            v = (ln(w - 0.5, 0.07) * (0.35 + 0.65 * fan) * (1.0 - smoothstep(0.55, 1.0, rho)) + exp(-sq(rho / 0.12)) * 0.9);
        }
        case 20: { // thorns: a spiky ring
            let tri = abs(fract((ang / tau + 1.0) * 11.0) - 0.5) * 2.0;
            v = ln(rho - (0.62 + 0.3 * (1.0 - tri)), 0.03) + ln(rho - 0.55, 0.025) * 0.6;
        }
        case 21: { // flames: tongues licking up out of a ring
            // Seven tongues licking up from a ring, each flickering at its own pace.
            let a7 = fract(ang / tau + 1.0 + t * 0.05) * 7.0;
            let cell = floor(a7);
            let h = 0.62 + 0.3 * hash(cell + seed) * (0.6 + 0.4 * sin(t * 9.0 + cell * 2.7));
            let edge = 0.42 + (h - 0.42) * pow(sin(fract(a7) * 3.14159), 2.5);
            v = ln(rho - edge, 0.025) + smoothstep(0.42, edge, rho) * step(rho, edge) * 0.45 + ln(rho - 0.42, 0.02) * 0.6;
        }
        case 22: { // goo: wobbling metaballs
            var s = 0.0;
            for (var i = 0; i < 3; i = i + 1) {
                let fi = f32(i);
                let c = vec2<f32>(cos(t * 1.3 + fi * 2.1), sin(t * 1.7 + fi * 1.3)) * 0.45;
                s = s + 0.06 / max(dot(uv - c, uv - c), 0.01);
            }
            v = smoothstep(0.9, 1.2, s) * 0.5 + ln(s - 1.0, 0.25) * 0.6;
        }
        default: {}
    }
    return clamp(v, 0.0, 1.6) * (1.0 - smoothstep(1.0, 1.15, rho));
}

// A type's texture on the ground (zones, ground strikes, burst footprints): (light, dim).
fn ground_tex(ty: i32, uv: vec2<f32>, t: f32, seed: f32) -> vec2<f32> {
    let rho = length(uv);
    let ang = atan2(uv.y, uv.x);
    switch ty {
        case 1: { // fire: a scorched floor with flickering embers
            let n = noise1(uv.x * 4.0 + t * 3.0 + seed) * 0.5 + noise1(uv.y * 4.0 - t * 4.0) * 0.5;
            return vec2<f32>(0.25 + 0.6 * smoothstep(0.2, 0.9, n), 0.25);
        }
        case 2: { // water: ripples
            return vec2<f32>(0.2 + 0.5 * pow(0.5 + 0.5 * sin(rho * 14.0 - t * 5.0), 4.0), 0.05);
        }
        case 3: { // electric: static crackling across the floor
            let tick = floor(t * 20.0);
            let c = floor(uv * 6.0);
            return vec2<f32>(0.1 + 1.2 * step(0.88, hash(c.x * 3.1 + c.y * 7.7 + tick + seed)), 0.0);
        }
        case 4: { // grass: a leafy carpet
            let c = fract(uv * 4.0 + vec2<f32>(sin(t), cos(t * 0.8)) * 0.1) - 0.5;
            return vec2<f32>(0.2 + 0.4 * ln(abs(c.x) - abs(c.y) * 0.5, 0.08), 0.0);
        }
        case 5: { // ice: crisp frost facets
            let c = floor(uv * 5.0);
            return vec2<f32>(0.3 + 0.6 * pow(hash(c.x * 5.3 + c.y * 9.1 + seed), 5.0), 0.0);
        }
        case 7: { // poison: bubbling ooze
            let g = uv * 4.0;
            let c = fract(g) - 0.5;
            let life = fract(t * 0.8 + hash(floor(g.x) * 3.0 + floor(g.y) * 5.0));
            return vec2<f32>(0.25 + 0.7 * ln(length(c) - life * 0.35, 0.05), 0.2);
        }
        case 8, 12: { // ground / rock: cracked earth and dust
            let w = (ang / 6.28318 + 1.0) * 7.0 + noise1(rho * 6.0 + seed) * 0.2;
            return vec2<f32>(0.12 + 0.5 * ln((fract(w) - 0.5) * rho, 0.03), 0.3);
        }
        case 9: { // flying: wind lines sweeping round
            return vec2<f32>(0.15 + 0.5 * pow(0.5 + 0.5 * sin(ang * 4.0 + rho * 3.0 - t * 6.0), 8.0), 0.0);
        }
        case 10: { // psychic: turning rings
            return vec2<f32>(0.2 + 0.5 * ln(fract(rho * 3.0 + t * 0.5) - 0.5, 0.08), 0.0);
        }
        case 13, 15: { // ghost / dark: ink that drinks the light
            return vec2<f32>(0.1, 0.45 + 0.15 * noise1(uv.x * 3.0 + uv.y * 2.0 + t));
        }
        case 16: { // steel: a polished plate with glints
            return vec2<f32>(0.2 + 1.0 * pow(hash(floor(uv.x * 4.0) + floor(uv.y * 4.0) * 7.0 + floor(t * 6.0)), 12.0), 0.0);
        }
        case 17: { // fairy: twinkles
            return vec2<f32>(0.2 + 1.2 * pow(hash(floor(uv.x * 7.0) * 3.0 + floor(uv.y * 7.0) * 11.0 + floor(t * 5.0)), 10.0), 0.0);
        }
        case 0: { // normal: clean sparkles
            return vec2<f32>(0.2 + 1.0 * pow(hash(floor(uv.x * 6.0) * 5.0 + floor(uv.y * 6.0) * 13.0 + floor(t * 4.0) + seed), 14.0), 0.0);
        }
        case 6: { // fighting: shock rings thudding outward
            return vec2<f32>(0.15 + 0.7 * ln(fract(rho * 1.5 - t * 1.6) - 0.5, 0.06), 0.1);
        }
        case 11: { // bug: a honeycomb
            let g = uv * 4.0;
            let hx = vec2<f32>(g.x + 0.5 * (floor(g.y) % 2.0), g.y);
            let c = abs(fract(hx) - 0.5);
            return vec2<f32>(0.15 + 0.5 * ln(max(c.x, c.y) - 0.45, 0.04), 0.05);
        }
        case 14: { // dragon: scales shimmering
            let c = fract(uv * 3.0) - 0.5;
            return vec2<f32>(0.2 + 0.4 * ln(length(c + vec2<f32>(0.0, 0.5)) - 0.5, 0.06), 0.0);
        }
        default: {
            return vec2<f32>(0.3, 0.0);
        }
    }
}

// Debris flung out of a point: one piece per angular cell (n cells), flying out to `dist` and
// falling a little. Returns the distance from p to this cell's piece, the cell's id, and the
// piece's centre.
fn debris(p: vec2<f32>, n: f32, dist: f32, fall: f32, seed: f32) -> vec4<f32> {
    let a = atan2(p.y, p.x) / 6.28318 + 0.5;
    let cell = floor(a * n);
    let ac = (cell + 0.25 + 0.5 * hash(cell + seed)) / n * 6.28318 - 3.14159;
    let dd = dist * (0.65 + 0.35 * hash(cell * 3.7 + seed));
    let c = vec2<f32>(cos(ac), sin(ac)) * dd - vec2<f32>(0.0, fall);
    return vec4<f32>(length(p - c), cell, c);
}

// A four-pointed glint: thin bright spikes along the axes, fading with distance.
fn glint(p: vec2<f32>, len: f32, w: f32) -> f32 {
    let sx = exp(-sq(p.y / w)) * exp(-abs(p.x) / len);
    let sy = exp(-sq(p.x / w)) * exp(-abs(p.y) / len);
    return sx + sy;
}

// A hit flash dressed by type, so a landing reads as the type that landed: (core, glow, white, dark)
// in a screen-facing disc where |p| = 1 is the flash's radius. e is 1 at the hit, 0 at the end.
fn hit_dress(ty: i32, p: vec2<f32>, age: f32, e: f32, time: f32, seed: f32) -> vec4<f32> {
    let rho = length(p);
    let ang = atan2(p.y, p.x);
    let grow = 1.0 - (1.0 - age) * (1.0 - age);
    var c = 0.0;
    var g = 0.0;
    var wh = 0.0;
    var dk = 0.0;
    switch ty {
        case 1: { // fire: a ragged fireball that blooms and licks upward
            let up = p - vec2<f32>(0.0, grow * 0.35);
            let edge = (0.45 + 0.5 * grow) * (1.0 + 0.3 * noise1(ang * 3.0 + time * 16.0 + seed) + 0.25 * max(up.y, 0.0));
            let d = length(up) - edge;
            g = smoothstep(0.1, -0.25, d) * 0.9 + ln(d, 0.08) * 0.6;
            c = exp(-sq(length(up) / 0.35)) * e;
        }
        case 2: { // water: a splash ring and droplets flung up and out
            let ring = ln(rho - (0.3 + 0.8 * grow), 0.07);
            let dr = debris(p, 8.0, 0.4 + 0.9 * grow, -0.3 * grow + 0.9 * grow * grow, seed);
            let drop = exp(-sq(dr.x / 0.07));
            wh = ring * 0.6 + drop * 0.8;
            g = ring * 0.5 + drop * 0.6 + exp(-sq(rho / 0.4)) * 0.4;
        }
        case 3: { // electric: forked sparks crackling out, re-forking 30 times a second
            let tick = floor(time * 30.0) + seed;
            let a6 = ang / 6.28318 * 7.0 + noise1(rho * 6.0 + tick) * 0.18;
            let arm = ln((fract(a6) - 0.5) * rho, 0.02) * step(rho, 0.5 + 0.9 * grow) * step(0.4, hash(floor(a6) + tick));
            c = arm * 1.4 + exp(-sq(rho / 0.18)) * e;
            g = arm * 0.6;
        }
        case 4: { // grass: leaves scattering out, spinning
            // Each leaf is an almond that tumbles (its long axis turns with time).
            let dr = debris(p, 7.0, 0.3 + 0.9 * grow, 0.2 * grow * grow, seed);
            let lp = rot2(p - dr.zw, time * 9.0 + dr.y);
            let leaf = length(vec2<f32>(lp.x * 0.45, lp.y * 1.4));
            let k = 1.0 - smoothstep(0.05, 0.075, leaf);
            g = k * 0.8 + exp(-sq(rho / 0.5)) * 0.3 * e;
            c = k * 0.5;
        }
        case 5: { // ice: a crisp six-pointed crystal and flying shards
            let a6 = abs(fract(ang / 6.28318 * 6.0) - 0.5) * 2.0;
            let star = (0.25 + 0.75 * pow(1.0 - a6, 3.0)) * (0.5 + 0.6 * grow);
            c = (1.0 - smoothstep(star - 0.03, star, rho)) * 0.7;
            wh = ln(rho - star, 0.025) * 0.8;
            let dr = debris(p, 9.0, 0.5 + 0.8 * grow, 0.1 * grow, seed);
            g = exp(-sq(dr.x / 0.05)) * 0.9 + c * 0.3;
        }
        case 6: { // fighting: a heavy shock ring and a starburst of short thick rays (a "pow")
            let rr = 0.3 + 1.0 * grow;
            let rays = pow(abs(cos(ang * 4.0 + seed)), 12.0) * step(rho, 0.3 + 0.8 * grow) * step(0.2, rho);
            c = ln(rho - rr, 0.09) * 1.1 + rays * 0.9;
            g = rays * 0.5 + exp(-sq(rho / 0.45)) * 0.5 * e;
        }
        case 7: { // poison: a wobbling goo splat with bubbles popping out of it
            let edge = (0.4 + 0.45 * grow) * (1.0 + 0.2 * noise1(ang * 2.5 + time * 3.0 + seed));
            let d = rho - edge;
            g = smoothstep(0.05, -0.2, d) * 0.55 + ln(d, 0.06) * 0.6;
            let dr = debris(p, 6.0, 0.6 + 0.7 * grow, -0.6 * grow, seed + 3.0);
            g = g + ln(dr.x - 0.09 * (1.0 - age), 0.025) * 0.9;
            dk = smoothstep(0.0, -0.2, d) * 0.15;
        }
        case 8: { // ground: a low cloud of dust kicked out sideways, dulling what's behind
            let q = vec2<f32>(p.x * 0.7, (p.y + 0.4 * grow) * 1.6);
            let edge = (0.5 + 0.7 * grow) * (1.0 + 0.25 * noise1(atan2(q.y, q.x) * 3.0 + seed + time * 2.0));
            let k = smoothstep(edge, edge * 0.4, length(q));
            g = k * 0.45;
            dk = k * 0.3;
            let dr = debris(p, 7.0, 0.5 + 0.8 * grow, 0.8 * grow * grow, seed);
            c = (1.0 - smoothstep(0.06, 0.09, dr.x)) * 0.35;
        }
        case 9: { // flying: two gusts curling round the point
            let sp = fract(ang / 6.28318 * 2.0 + rho * 0.9 - time * 2.5);
            let band = ln(sp - 0.5, 0.06) * smoothstep(0.15, 0.35, rho) * (1.0 - smoothstep(0.6 + 0.5 * grow, 0.9 + 0.5 * grow, rho));
            g = band * 0.9;
            wh = band * 0.35;
            c = exp(-sq(rho / 0.25)) * 0.5 * e;
        }
        case 10: { // psychic: rings collapsing inward onto the point, then a bright pop
            let k = fract(age * 3.0);
            g = ln(rho - (1.2 - k), 0.05) * 0.9 + ln(rho - (1.2 - fract(age * 3.0 + 0.5)) , 0.05) * 0.5;
            c = exp(-sq(rho / (0.15 + 0.35 * grow))) * 0.9;
        }
        case 11: { // bug: a swarm of specks buzzing out
            let dr = debris(p, 12.0, 0.3 + 0.9 * grow, 0.0, seed + floor(time * 12.0) * 0.01);
            let dr2 = debris(rot2(p, 0.26), 12.0, 0.2 + 0.6 * grow, 0.0, seed + 5.0);
            c = (exp(-sq(dr.x / 0.045)) + exp(-sq(dr2.x / 0.04))) * 1.1;
            g = c * 0.4 + exp(-sq(rho / 0.35)) * 0.3 * e;
        }
        case 12: { // rock: chunky debris thrown out and falling, a dull thud
            let dr = debris(p, 5.0, 0.35 + 0.8 * grow, 0.9 * grow * grow, seed);
            let q = abs(rot2(p - dr.zw, dr.y * 1.3 + age * 6.0));
            let sz = 0.11 + 0.06 * hash(dr.y + seed);
            let chunk = max(q.x, q.y * 1.2) - sz;
            c = (1.0 - smoothstep(-0.01, 0.015, chunk)) * (0.3 + 0.2 * hash(dr.y * 5.0 + seed));
            g = c * 0.3 + ln(rho - (0.3 + 0.9 * grow), 0.08) * 0.3;
            dk = (1.0 - smoothstep(0.0, 0.03, chunk)) * 0.25;
        }
        case 13: { // ghost: a dark hollow that swallows the light, wisps curling round it
            let sp = fract(ang / 6.28318 * 3.0 - rho * 1.2 + time * 1.2);
            g = ln(sp - 0.5, 0.12) * smoothstep(0.3, 0.6, rho) * (1.0 - smoothstep(0.8, 1.3, rho)) * 0.7;
            dk = exp(-sq(rho / (0.4 + 0.5 * grow))) * 0.55;
        }
        case 14: { // dragon: three claw rakes torn across it, with a violet flare
            let q = rot2(p, 0.7 + seed * 0.3);
            let lane = fract(q.x * 2.4 + 0.5) - 0.5;
            let rake = ln(lane, 0.07) * step(abs(q.x), 0.62) * (1.0 - smoothstep(0.7, 1.0, abs(q.y))) * step(q.y, -1.0 + 2.6 * min(age * 4.0, 1.0));
            c = rake * 1.2;
            g = rake * 0.6 + exp(-sq(rho / 0.5)) * 0.5 * e;
        }
        case 15: { // dark: a jagged black cross-cut that drains the light, red at its edges
            let q = rot2(p, 0.8);
            let jag = 0.05 * noise1(q.y * 9.0 + seed);
            let cut = min(abs(q.x + jag), abs(rot2(q, 1.4).x - jag)) ;
            let slash = ln(cut, 0.06) * (1.0 - smoothstep(0.7, 1.1, rho));
            c = slash * 0.7;
            dk = exp(-sq(rho / 0.7)) * 0.5 + slash * 0.2;
        }
        case 16: { // steel: a hard metallic glint with long thin rays and a sharp ring
            c = glint(rot2(p, 0.785), 0.35 + 0.4 * e, 0.035) * 1.2;
            wh = ln(rho - (0.25 + 0.6 * grow), 0.02) * 0.7 + glint(p, 0.2, 0.02) * 0.6 * e;
            g = exp(-sq(rho / 0.35)) * 0.4;
        }
        case 17: { // fairy: twinkling sparkles scattering, and a soft pink bloom
            let dr = debris(p, 8.0, 0.35 + 0.8 * grow, -0.2 * grow, seed);
            let tw = 0.5 + 0.5 * sin(time * 25.0 + dr.y * 2.0);
            c = exp(-sq(dr.x / 0.05)) * (0.6 + 0.8 * tw);
            g = exp(-sq(rho / 0.6)) * 0.55 + c * 0.3;
            wh = exp(-sq(dr.x / 0.025)) * tw * 0.6;
        }
        default: { // normal: a clean white four-point star pop
            c = glint(p, 0.25 + 0.35 * e, 0.06) * 1.1 + exp(-sq(rho / 0.25)) * e;
            g = ln(rho - (0.3 + 0.7 * grow), 0.05) * 0.5;
        }
    }
    return vec4<f32>(c, g, wh, dk) * e;
}

// A projectile orb dressed by type, for the types without a look of their own in the orb code:
// (core, glow, white, dark) in a screen-facing disc where |p| = 1 is the orb's radius r; tv is the
// direction it travels on screen (zero when it flies straight at the camera).
fn orb_dress(ty: i32, p: vec2<f32>, tv: vec2<f32>, time: f32, seed: f32) -> vec4<f32> {
    let rho = length(p);
    let ang = atan2(p.y, p.x);
    // Along and across the flight on screen (x forward).
    var fwd = vec2<f32>(1.0, 0.0);
    if (dot(tv, tv) > 1e-4) { fwd = normalize(tv); }
    let fp = vec2<f32>(dot(p, fwd), fwd.x * p.y - fwd.y * p.x);
    var c = 0.0;
    var g = 0.0;
    var wh = 0.0;
    var dk = 0.0;
    switch ty {
        case 4: { // grass: a spinning leaf blade, a second one trailing
            let l1 = rot2(p, time * 14.0 + seed);
            let leaf = length(vec2<f32>(l1.x * 0.5, l1.y * 1.6));
            c = (1.0 - smoothstep(0.5, 0.58, leaf)) * 0.8;
            let vein = ln(l1.y, 0.04) * step(abs(l1.x), 0.9);
            wh = vein * c * 0.4;
            let l2 = rot2(fp + vec2<f32>(1.3, 0.0), -time * 11.0);
            g = (1.0 - smoothstep(0.3, 0.36, length(vec2<f32>(l2.x * 0.55, l2.y * 1.6)))) * 0.7 + exp(-sq(rho / 0.9)) * 0.25;
        }
        case 5: { // ice: a hard hexagonal crystal with a white edge and a glint
            let a6 = abs(fract((ang + time * 2.0) / 6.28318 * 6.0) - 0.5);
            let hexr = 0.62 / cos(a6 * 1.0472);
            c = (1.0 - smoothstep(hexr - 0.04, hexr, rho)) * 0.6;
            wh = ln(rho - hexr, 0.05) * 0.9 + glint(rot2(p, 0.6), 0.3, 0.05) * 0.5;
            g = exp(-sq(rho / 0.8)) * 0.25;
        }
        case 6: { // fighting: a fist of force with shock rings beating round it
            let k = fract(time * 6.0 + seed);
            c = exp(-sq(rho / 0.5)) * 1.1;
            g = ln(rho - (0.6 + 0.6 * k), 0.08) * (1.0 - k) * 1.2 + exp(-sq(rho / 0.9)) * 0.3;
        }
        case 7: { // poison: a wobbling glob of sludge dripping behind it
            let edge = 0.6 * (1.0 + 0.22 * noise1(ang * 2.0 + time * 6.0 + seed));
            c = (1.0 - smoothstep(edge - 0.1, edge, rho)) * 0.55;
            g = ln(rho - edge, 0.08) * 0.8;
            let drip = fp + vec2<f32>(1.0 + fract(time * 3.0) * 0.6, 0.15 * sin(time * 9.0));
            g = g + exp(-sq(length(drip) / 0.16)) * 0.8 + exp(-sq(length(fp + vec2<f32>(0.75, -0.2)) / 0.12)) * 0.6;
            dk = c * 0.2;
        }
        case 9: { // flying: a crescent air blade, curved forward, with a pale wake
            let d = abs(length(fp + vec2<f32>(0.5, 0.0)) - 1.0);
            let blade = ln(d, 0.08 + 0.1 * smoothstep(1.0, 0.0, abs(fp.y))) * step(-0.5, fp.x) * step(abs(fp.y), 1.0);
            c = blade * 0.9;
            wh = blade * 0.4;
            g = blade * 0.4 + ln(abs(fp.y) - 0.45, 0.06) * step(fp.x, -0.2) * exp(fp.x * 0.8) * 0.5;
        }
        case 10: { // psychic: a bubble with a ring turning inside it, pulsing
            wh = ln(rho - 0.75, 0.08) * 0.5;
            let tilt = vec2<f32>(p.x, p.y * (1.8 + 0.8 * sin(time * 7.0)));
            c = ln(length(rot2(tilt, time * 3.0)) - 0.5, 0.06) * 0.9 + exp(-sq(rho / 0.25)) * 0.6;
            g = exp(-sq(rho / 0.8)) * 0.4;
        }
        case 11: { // bug: a cloud of specks buzzing round a small core
            let jit = floor(time * 14.0) * 0.137;
            let d1 = debris(p, 6.0, 1.0, 0.0, seed + jit);
            let d2 = debris(rot2(p, 0.5), 5.0, 0.55, 0.0, seed + jit + 3.0);
            c = (exp(-sq(d1.x / 0.11)) + exp(-sq(d2.x / 0.1))) * 1.1 + exp(-sq(rho / 0.25)) * 0.5;
            g = c * 0.3;
        }
        case 14: { // dragon: a violet fireball with two sparks orbiting it
            let rag = 1.0 + 0.25 * noise1(ang * 3.0 + time * 14.0 + seed);
            c = exp(-sq(rho / (0.5 * rag))) * 1.1;
            g = exp(-sq(rho / (0.95 * rag))) * 0.7;
            let a = time * 12.0 + seed;
            let s1 = vec2<f32>(cos(a), sin(a) * 0.45) * 1.05;
            wh = (exp(-sq(length(p - s1) / 0.12)) + exp(-sq(length(p + s1) / 0.12))) * 0.8;
        }
        case 16: { // steel: a hard polished ball, a sharp edge and a turning glint
            c = (1.0 - smoothstep(0.58, 0.64, rho)) * (0.35 + 0.35 * smoothstep(0.6, -0.4, p.y));
            wh = ln(rho - 0.61, 0.035) * 0.8 + glint(rot2(p - vec2<f32>(-0.22, 0.22), 0.785), 0.9, 0.03) * (0.5 + 0.7 * pow(0.5 + 0.5 * sin(time * 9.0 + seed), 4.0));
            g = exp(-sq(rho / 0.75)) * 0.15;
        }
        case 17: { // fairy: a soft pink bloom with four twinkles circling it
            c = exp(-sq(rho / 0.4)) * 0.8;
            g = exp(-sq(rho / 0.9)) * 0.6;
            let a = time * 5.0 + seed;
            let q = rot2(p, -a);
            let cell = floor((atan2(q.y, q.x) / 6.28318 + 0.5) * 4.0);
            let ac = (cell + 0.5) / 4.0 * 6.28318 - 3.14159;
            let sp = q - vec2<f32>(cos(ac), sin(ac)) * 1.05;
            let tw = 0.6 + 0.4 * sin(time * 20.0 + cell * 1.7);
            wh = glint(sp, 0.12 * tw, 0.025) * 1.2;
        }
        default: { // normal: a clean white ball with a turning four-point star
            c = exp(-sq(rho / 0.5)) * 1.1;
            wh = glint(rot2(p, time * 6.0), 0.75, 0.06) * 0.7;
            g = exp(-sq(rho / 0.9)) * 0.4;
        }
    }
    return vec4<f32>(c, g, wh, dk);
}

// One crescent of a melee slash: the band at radius `rad` (|uv| = 1 is the reach's swing), thick
// `tk` in the middle and tapering to its ends, drawn where the sweep has already passed.
fn arc(uv: vec2<f32>, rad: f32, tk: f32, lead: f32) -> f32 {
    let th = atan2(uv.y, uv.x);
    let span = smoothstep(lead - 0.15, lead, th) * (1.0 - smoothstep(2.3, 2.5, th)) * step(-0.8, th);
    let thick = tk * (0.45 + 0.55 * sin(clamp((2.4 - th) / 3.0, 0.0, 1.0) * 3.14159));
    return (1.0 - smoothstep(thick * 0.6, thick, abs(length(uv) - rad))) * span;
}

// A melee slash dressed by type, so a claw, a karate chop and a leaf blade read apart:
// (core, glow, white, dark). uv is in the slash plane, scaled so the swing's radius is 1.
fn slash_dress(ty: i32, uv: vec2<f32>, age: f32, time: f32, seed: f32) -> vec4<f32> {
    let sweep = clamp(age * 5.0, 0.0, 1.0);
    let lead = 2.4 - 3.0 * sweep;
    let rho = length(uv);
    let th = atan2(uv.y, uv.x);
    let span = smoothstep(lead - 0.15, lead, th) * (1.0 - smoothstep(2.3, 2.5, th)) * step(-0.8, th);
    let halo = exp(-abs(rho - 1.0) / 0.1) * span;
    var c = 0.0;
    var g = 0.0;
    var wh = 0.0;
    var dk = 0.0;
    switch ty {
        case 1: { // fire: a thick arc whose outer edge licks into flame
            let flame = max(0.0, noise1(th * 7.0 - time * 22.0 + seed)) * 0.18;
            let b = arc(uv * (1.0 - flame * 0.5), 1.0, 0.2 + flame, lead);
            c = b * 0.5;
            g = b * 1.0 + halo * 0.6;
        }
        case 2: { // water: an arc with a white foam crest and spray thrown off it
            let b = arc(uv, 1.0, 0.16, lead);
            wh = ln(rho - 1.08, 0.03) * span * 0.8;
            let sp = debris(uv, 14.0, 1.3, 0.0, seed);
            wh = wh + exp(-sq(sp.x / 0.035)) * span * 0.7;
            c = b * 0.35;
            g = b * 0.8 + halo * 0.4;
        }
        case 3: { // electric: a thin arc that jitters and re-forms 30 times a second
            let tick = floor(time * 30.0) + seed;
            let j = noise1(th * 9.0 + tick * 3.1) * 0.07;
            let b = arc(uv * (1.0 + j), 1.0, 0.06, lead);
            c = b * 1.1;
            g = b * 0.8 + halo * 0.5 * (0.5 + hash(tick));
        }
        case 4: { // grass: a pointed leaf blade, with leaves shed behind the sweep
            let b = arc(uv, 1.0, 0.22, lead) * sqrt(sin(clamp((2.4 - th) / 3.0, 0.0, 1.0) * 3.14159));
            let lf = debris(uv, 10.0, 1.05, 0.15 + age * 0.4, seed);
            let leaf = 1.0 - smoothstep(0.05, 0.08, length(rot2(uv - lf.zw, lf.y + time * 8.0) * vec2<f32>(0.5, 1.5)));
            c = b * 0.4;
            g = b * 0.9 + leaf * span * 0.9;
        }
        case 5: { // ice: an arc of separate crystal shards, crisp, white-edged
            let cell = fract(th * 4.0 + seed);
            let shard = step(0.2, cell) * step(cell, 0.85);
            let b = arc(uv, 1.0, 0.16 * (1.2 - abs(cell - 0.5)), lead) * shard;
            c = b * 0.6;
            wh = b * 0.4;
            g = halo * 0.25;
        }
        case 6: { // fighting: a short heavy chop and a shock ring where it lands
            let b = arc(uv, 1.0, 0.3, lead) * step(0.5, th);
            let hit = vec2<f32>(cos(0.5), sin(0.5));
            let k = clamp(age * 4.0 - 0.6, 0.0, 1.0);
            let ring = ln(length(uv - hit) - 0.15 - k * 0.55, 0.05) * step(0.01, k) * (1.0 - k);
            c = b * 0.6 + ring * 1.1;
            g = b * 1.0 + ring * 0.6;
        }
        case 7: { // poison: an arc that drips sludge down from it
            let b = arc(uv, 1.0, 0.16, lead);
            // Drips hang straight down (the plane is turned 0.6 rad) from points along the arc.
            let down = rot2(vec2<f32>(0.0, -1.0), 0.6);
            let cell = floor(th * 5.0 + seed);
            let ac = (cell + 0.5 - seed) / 5.0;
            let rel = uv - vec2<f32>(cos(ac), sin(ac));
            let along = dot(rel, down);
            let fall = age * 1.6 * (0.5 + hash(cell));
            let drip = ln(rel.x * down.y - rel.y * down.x, 0.035) * step(0.0, along) * smoothstep(fall + 0.05, fall, along) * step(0.3, hash(cell + 3.0));
            c = b * 0.4;
            g = b * 0.9 + halo * 0.4 + drip * span * 0.7;
        }
        case 8: { // ground: a broad, gritty, dull swipe that kicks up dust
            let grit = 0.6 + 0.4 * hash(floor(th * 20.0) + floor(rho * 12.0) + seed);
            let b = arc(uv, 1.0, 0.28, lead) * grit;
            c = b * 0.3;
            g = b * 0.5;
            dk = b * 0.2;
        }
        case 9: { // flying: two thin wind blades, one just behind the other
            let b = arc(uv, 0.88, 0.05, lead) + arc(uv, 1.14, 0.05, lead + 0.25);
            c = b * 0.7;
            wh = b * 0.3;
            g = b * 0.5;
        }
        case 10: { // psychic: an arc with a fainter echo pulsing outward behind it
            let b = arc(uv, 1.0, 0.13, lead);
            let e = arc(uv, 1.15 + 0.2 * fract(time * 3.0), 0.05, lead + 0.4) * (1.0 - fract(time * 3.0));
            c = b * 0.45;
            g = b * 0.9 + e * 0.8 + halo * 0.4;
        }
        case 11: { // bug: two thin scissor cuts crossing
            let b = arc(uv, 1.0, 0.06, lead) + arc(vec2<f32>(-uv.x, uv.y) + vec2<f32>(0.25, 0.0), 1.0, 0.06, lead);
            c = b * 0.8;
            g = b * 0.6;
        }
        case 12: { // rock: chunky blocks swung in an arc, little glow
            let cell = fract(th * 3.0 + seed);
            let b = arc(uv, 1.0, 0.3, lead) * step(0.25, cell) * step(cell, 0.9);
            c = b * 0.45;
            g = b * 0.2;
            dk = b * 0.15;
        }
        case 13: { // ghost: a faint wavering arc that dims the scene behind it
            let wv = 0.06 * sin(th * 5.0 - time * 8.0);
            let b = arc(uv * (1.0 + wv), 1.0, 0.18, lead);
            c = b * 0.2;
            g = b * 0.6 + halo * 0.3;
            dk = b * 0.35;
        }
        case 14: { // dragon: three claw rakes side by side
            let b = arc(uv, 0.8, 0.055, lead) + arc(uv, 1.0, 0.065, lead) + arc(uv, 1.2, 0.055, lead);
            c = b * 0.8;
            g = b * 0.7;
        }
        case 15: { // dark: a jagged black cut, red only at its edge
            let ju = uv * (1.0 + noise1(th * 6.0 + seed) * 0.06);
            let b = arc(ju, 1.0, 0.2, lead);
            let edge = max(arc(ju, 1.0, 0.26, lead) - arc(ju, 1.0, 0.15, lead), 0.0);
            c = edge * 0.8;
            g = edge * 0.6;
            dk = b * 0.55;
        }
        case 16: { // steel: a razor-thin bright edge with a glint riding its leading tip
            let b = arc(uv, 1.0, 0.07, lead);
            let tip = uv - vec2<f32>(cos(lead), sin(lead));
            c = b * 0.7;
            wh = b * 0.5 + glint(rot2(tip, 0.785), 0.25, 0.025) * (1.0 - sweep * 0.7);
            g = halo * 0.2;
        }
        case 17: { // fairy: a soft pink arc with sparkles left along it
            let b = arc(uv, 1.0, 0.13, lead);
            let sp = debris(uv, 12.0, 1.05, 0.0, seed);
            let tw = 0.5 + 0.5 * sin(time * 20.0 + sp.y * 2.3);
            c = b * 0.35;
            g = b * 0.8 + halo * 0.4;
            wh = exp(-sq(sp.x / 0.04)) * span * tw * 0.9;
        }
        default: { // normal: one clean crescent
            let b = arc(uv, 1.0, 0.16, lead);
            c = b * 0.45;
            g = b * 0.9 + halo * 0.5;
        }
    }
    return vec4<f32>(c, g, wh, dk);
}

fn perp(d: vec3<f32>) -> vec3<f32> {
    var up = vec3<f32>(0.0, 1.0, 0.0);
    if (abs(d.y) > 0.9) { up = vec3<f32>(1.0, 0.0, 0.0); }
    return normalize(cross(d, up));
}

// Each type has its own motion and material (move effects spec, "Type signatures"):
// 0 normal, 1 fire, 2 water, 3 electric, 4 grass, 5 ice, 6 fighting, 7 poison, 8 ground, 9 flying,
// 10 psychic, 11 bug, 12 rock, 13 ghost, 14 dragon, 15 dark, 16 steel, 17 fairy. Flags (bits):
// bite 1, punch 2, slicing 4, pulse 8, bullet 16, wind 32, sound 64. Returns light (rgb) and how
// much to dim the scene behind (a).
fn one(f: Fx, rd: vec3<f32>, tmax: f32, time: f32) -> vec4<f32> {
    let shape = i32(f.b.w + 0.5);
    let r = f.a.w;
    let age = f.p.x;
    let seed = f.p.y;
    let fl = i32(floor(f.p.z / 32.0 + 0.001));
    let ty = i32(f.p.z + 0.5) - fl * 32;
    var col = f.color.rgb;
    var glow = 0.0;
    var core = 0.0;
    var white = 0.0;
    var dark = 0.0;
    // Cheap early out: the view ray misses the effect's bounding sphere (ground shapes use the
    // scene point, so the same sphere bounds them).
    let bc = (f.a.xyz + f.b.xyz) * 0.5;
    let br = length(f.b.xyz - f.a.xyz) * 0.5 + r * 2.5 + 2.0;
    let btc = clamp(dot(bc, rd), 0.0, tmax);
    if (length(rd * btc - bc) > br) { return vec4<f32>(0.0); }
    // Signature layer: motif (sig.rs MOTIFS), grand (a final stage's version), accent colour.
    let mw0 = i32(f.m.w + 0.5);
    let arch = mw0 / 64;
    let mw = mw0 % 64;
    let grand = mw >= 32;
    let mk = mw - select(0, 32, grand);
    var mot = 0.0;
    if (shape == 0 || shape == 1 || shape == 6) {
        // Beam (0), dash streak (1) or forward cone (6, r is its radius at the far end).
        var a = f.a.xyz;
        var b = f.b.xyz;
        var env = 1.0 - smoothstep(0.55, 1.0, age);
        if (shape == 1) {
            // The streak's head runs from a to b in the first third, its tail follows.
            let head = clamp(age * 3.0, 0.0, 1.0);
            // The tail stops short of the head: a zero-length streak (age past 0.78) made NaNs
            // that drew solid white domes for the rest of the dash.
            let tail = clamp(age * 1.6 - 0.25, 0.0, 0.94);
            let ab = b - a;
            b = a + ab * head;
            a = a + ab * tail;
        } else {
            env = env * smoothstep(0.0, 0.04, age);
        }
        let q0 = ray_seg(rd, tmax, a, b);
        var q = q0;
        // Far from the line (past every type's widest glow, wobble and slash), nothing shows:
        // skip the rest. The bounding sphere of a long beam near the camera covers the whole screen.
        if (shape != 6 && length(q.xyz - (a + (b - a) * q.w)) > max(r, 0.3) * 6.0) { return vec4<f32>(0.0); }
        let ab = b - a;
        // A cone is a volume, not a line: from behind the caster every view ray runs almost along
        // its axis, where the closest point to a line is meaningless (it drew a thin stalk). March
        // the ray through it instead: how much cone it crosses, and where along it, on average.
        var vol = 1.0;
        if (shape == 6) {
            let clen = max(length(ab), 0.1);
            let cdir = ab / clen;
            let cc = (a + b) * 0.5;
            let cr = clen * 0.5 + r;
            let tcc = dot(cc, rd);
            let hh = length(rd * tcc - cc);
            if (hh > cr) { return vec4<f32>(0.0); }
            let half = sqrt(cr * cr - hh * hh);
            let t0 = max(tcc - half, 0.0);
            let t1 = min(tcc + half, tmax);
            if (t1 <= t0) { return vec4<f32>(0.0); }
            let dt = (t1 - t0) / 8.0;
            var acc = 0.0;
            var ax = 0.0;
            for (var i = 0; i < 8; i = i + 1) {
                let pt = rd * (t0 + (f32(i) + 0.5) * dt);
                let xx = dot(pt - a, cdir);
                if (xx > 0.0 && xx < clen) {
                    let rad = max(r * xx / clen, 0.12);
                    let k = exp(-sq(length(pt - a - cdir * xx) / rad) * 1.6) * dt / rad;
                    acc = acc + k;
                    ax = ax + k * xx;
                }
            }
            if (acc < 1e-3) { return vec4<f32>(0.0); }
            vol = 1.0 - exp(-acc * 0.5);
            let xm = ax / acc;
            q = vec4<f32>(a + cdir * xm, xm / clen);
        }
        let len = length(ab);
        // A zero-length line (a dash blocked at once) has no direction: perp() of a zero vector
        // is NaN, which drew solid white domes. Fall back to straight ahead.
        let dir = select(vec3<f32>(0.0, 0.0, 1.0), ab / max(len, 1e-4), len > 1e-4);
        let u1 = perp(dir);
        let u2 = cross(dir, u1);
        let x = q.w * len;
        var on = a + ab * q.w;
        var on2 = on;
        var two = false;
        var w = r;
        var mask = 1.0;
        var glowk = 0.45;
        var corek = 1.0;
        var crisp = false;
        var darkk = 0.0;
        if (ty == 3) {
            if (len > 0.01) {
                // Electric: a thin bolt that zigzags and re-forks 30 times a second.
                let tick = floor(time * 30.0) + seed;
                let k = x * 2.2 + tick * 7.13;
                let jag = noise1(k) * 0.7 + noise1(k * 3.1 + 9.0) * 0.3;
                let jag2 = noise1(k + 51.7) * 0.7 + noise1(k * 3.1 + 77.0) * 0.3;
                let amp = 0.3 * sin(3.14159 * q.w) * min(r * 2.0, 1.0);
                on = on + (u1 * jag + u2 * jag2) * amp;
                env = env * (0.6 + 0.4 * hash(tick));
                w = r * 0.3;
            }
        } else if (ty == 1) {
            // Fire: licks and flickers, widens as it goes, rises a little.
            let n = noise1(x * 1.7 - time * 9.0 + seed);
            on = on + u1 * n * 0.18 * r + vec3<f32>(0.0, 1.0, 0.0) * q.w * 0.3 * r * (0.6 + 0.4 * noise1(x * 2.3 - time * 7.0));
            w = r * (0.55 + 0.9 * q.w) * (0.85 + 0.15 * noise1(x * 3.0 - time * 14.0));
            glowk = 0.6;
            col = mix(col, vec3<f32>(1.0, 0.8, 0.2), 0.35 * (1.0 - q.w));
        } else if (ty == 2) {
            // Water: a wobbling stream that sags, with a white foam edge.
            on = on + (u1 * sin(x * 2.5 - time * 14.0) + u2 * cos(x * 2.1 - time * 11.0)) * 0.08 * r;
            on.y = on.y - sin(3.14159 * q.w) * 0.04 * len;
            white = 1.0;
            glowk = 0.35;
        } else if (ty == 4) {
            // Grass: a spiral of leaves in short bursts.
            let phi = x * 3.5 - time * 9.0 + seed;
            on = on + (u1 * cos(phi) + u2 * sin(phi)) * r * 0.55;
            w = r * 0.4;
            mask = 0.35 + 0.65 * step(0.45, fract(x * 2.5 - time * 5.0));
        } else if (ty == 5) {
            // Ice: hard-edged and straight, little bloom, glinting facets.
            crisp = true;
            glowk = 0.18;
            mask = 0.8 + 0.6 * pow(hash(floor(x * 4.0) + seed), 6.0);
            col = mix(col, vec3<f32>(0.9, 1.0, 1.0), 0.3);
        } else if (ty == 6) {
            // Fighting: heavy shock rings travel down it.
            mask = 0.5 + 1.0 * exp(-sq((fract(x * 0.6 - time * 3.0) - 0.5) * 7.0));
            w = r * 1.1;
            glowk = 0.3;
        } else if (ty == 7) {
            // Poison: thick, blobby, slow.
            w = r * (0.6 + 0.55 * (0.5 + 0.5 * noise1(x * 1.3 - time * 2.5 + seed)));
            on = on + u2 * noise1(x * 0.9 - time * 1.5) * 0.15 * r;
            glowk = 0.4;
        } else if (ty == 8) {
            // Ground: a broad gritty dust stream, no glow.
            w = r * 1.25;
            glowk = 0.1;
            corek = 0.55;
            mask = 0.55 + 0.45 * hash(floor(x * 4.0) + floor(time * 12.0) + seed);
        } else if (ty == 9) {
            // Flying: two thin curved streamlines of pale air.
            let phi = x * 1.4 - time * 6.0;
            on = on + (u1 * cos(phi) + u2 * sin(phi)) * r * 0.5;
            on2 = a + ab * q.w - (u1 * cos(phi) + u2 * sin(phi)) * r * 0.5;
            two = true;
            w = r * 0.3;
            glowk = 0.25;
            corek = 0.6;
        } else if (ty == 10) {
            // Psychic: rings that pulse along it.
            w = r * 0.8;
            mask = 0.3 + 1.1 * exp(-sq((fract(x * 1.2 - time * 2.0) - 0.5) * 10.0));
        } else if (ty == 11) {
            // Bug: a buzzing swarm of specks.
            let cell = floor(x * 5.0);
            mask = step(0.5, hash(cell * 1.7 + seed + floor(time * 8.0)));
            on = on + (u1 * (hash(cell) - 0.5) + u2 * (hash(cell + 3.1) - 0.5)) * r * 0.9;
            w = r * 0.3;
            glowk = 0.2;
        } else if (ty == 12) {
            // Rock: chunky segments, no glow.
            mask = 0.35 + 0.65 * step(0.35, fract(x * 1.6));
            w = r * 1.15;
            glowk = 0.1;
            corek = 0.7;
            crisp = true;
        } else if (ty == 13) {
            // Ghost: drifting wisps that darken the world around them.
            on = on + (u1 * noise1(x * 0.8 - time * 2.0) + u2 * noise1(x * 0.7 - time * 1.7 + 5.0)) * 0.3 * r;
            w = r * 1.3;
            glowk = 0.3;
            corek = 0.35;
            darkk = 0.55;
        } else if (ty == 14) {
            // Dragon: a twisting double helix.
            let phi = x * 2.6 - time * 8.0;
            let c = a + ab * q.w;
            on = c + (u1 * cos(phi) + u2 * sin(phi)) * r * 0.6;
            on2 = c - (u1 * cos(phi) + u2 * sin(phi)) * r * 0.6;
            two = true;
            w = r * 0.45;
            glowk = 0.6;
            corek = 1.2;
        } else if (ty == 15) {
            // Dark: an ink stream that drains the light, red at the edge.
            w = r * 1.1;
            darkk = 0.7;
            corek = 0.25;
            glowk = 0.3;
            col = vec3<f32>(0.9, 0.12, 0.15);
        } else if (ty == 16) {
            // Steel: hard and mirror-bright, sharp glints.
            crisp = true;
            glowk = 0.2;
            mask = 1.0 + 2.0 * pow(hash(floor(x * 3.0) + floor(time * 20.0) + seed), 12.0);
        } else if (ty == 17) {
            // Fairy: soft pastel with twinkling sparkles.
            mask = 0.5 + 1.6 * pow(hash(floor(x * 6.0) + floor(time * 10.0) + seed), 8.0);
            glowk = 0.55;
        } else {
            // Normal: clean and snappy, a bright core and little glow.
            corek = 1.2;
            glowk = 0.28;
        }
        // Flag modifiers.
        if ((fl & (8 | 64)) != 0) {
            // Pulse / sound: rings travel down the line instead of a solid stream.
            // (0.35 floor: at 0.2 Bug Buzz and Dragon Pulse read too faint.)
            mask = mask * (0.35 + 1.4 * exp(-sq((fract(x * 0.5 - time * 3.0) - 0.5) * 8.0)));
            if ((fl & 64) != 0) { w = w * 1.4; }
        }
        if ((fl & 32) != 0) {
            // Wind: thinner curving air (glow kept: fainter made Gust vanish).
            w = w * 0.7;
            on = on + u1 * sin(x * 1.2 - time * 7.0) * r * 0.5;
        }
        // Archetype (line shape only): what kind of thing travels down the line.
        var orb = 0.0;
        if (shape == 0 && arch == 1) {
            // Projectile: an orb flies from a to b in the first eighth (the server's hit is
            // instant, so it must arrive about when the hit flash does), leaving a thin fading
            // trail; nothing is ahead of it.
            let h = clamp(age * 8.0, 0.0, 1.0);
            let dh = (q.w - h) * len;
            let tail = smoothstep(-max(len * 0.45, 1.0), 0.0, dh) * (1.0 - smoothstep(-0.05, 0.1, dh));
            mask = mask * tail * 0.8;
            w = w * 0.5;
            orb = h;
        } else if (shape == 0 && arch == 2) {
            // Melee: the line is only a thrust (all a side-on viewer sees); the slash is drawn below.
            mask = mask * 0.5;
            orb = -1.0;
        } else if (shape == 0 && arch == 3) {
            // Beam: a steady thick ray with a white-hot core.
            w = w * 1.25;
            corek = corek * 1.5;
            white = max(white, 0.6);
        }
        if (shape == 6) {
            // A cone widens from nothing at the mouth, and its front sweeps out early on.
            w = w * max(q.w, 0.08);
            let front = clamp(age * 2.5, 0.0, 1.0);
            mask = mask * (1.0 - smoothstep(front - 0.08, front, q.w));
        }
        var d = length(q.xyz - on);
        if (two) { d = min(d, length(q.xyz - on2)); }
        if (mk > 0) {
            // Signature: the motif stamped along the line, facing the camera, scrolling with it.
            let span = max(r * 2.6, 0.8);
            let cell = (fract(x / span - time * 0.8) - 0.5) * 2.0;
            let off = q0.xyz - (a + ab * q0.w);
            let side = sign(dot(off, u1) + 1e-4) * length(off) / max(r * 1.3, 0.3);
            var stamp = motif(mk, vec2<f32>(cell, side), time, seed);
            if (shape == 6) { stamp = stamp * max(q.w, 0.15); }
            // Only along the line's body: past its ends the closest point sticks to the end and the
            // stamp would wrap round it in rings.
            stamp = stamp * smoothstep(0.0, 0.08, q.w) * (1.0 - smoothstep(0.92, 1.0, q.w)) * 0.65;
            mot = stamp * (1.0 - smoothstep(0.55, 1.0, age)) * smoothstep(0.0, 0.05, age);
        }
        var along = 1.0;
        if (shape == 1) { along = 0.25 + 0.75 * q.w; }
        // Seen from inside (the beam passes through the camera), only the core shows.
        let ca = clamp(dot(-a, ab) / max(dot(ab, ab), 1e-6), 0.0, 1.0);
        let inside = smoothstep(r * 0.5, r * 4.0, length(a + ab * ca));
        if (crisp) {
            core = (1.0 - smoothstep(w * 0.3, w * 0.45, d)) * 0.9;
        } else {
            core = exp(-sq(d / (w * 0.35)));
        }
        core = core * env * along * corek * mask;
        glow = exp(-d / max(w, 0.05)) * glowk * env * along * inside * mask;
        if (shape == 6) {
            // The cone's own body: as bright as the stretch of it the ray crossed (no "inside" fade,
            // the caster always stands at its mouth). The type reads from its pattern along the
            // cone (rings, segments, flicker), not from a wobble round the axis, which a wide cone
            // smears out. Kept translucent: the caster looks through it.
            // Streaks fanning out from the mouth (by the view ray's angle round the axis) so it
            // reads as a spray, not a lamp shade: jagged and flickering for electric, swirling for
            // flying and dragon, soft for the rest.
            let va = rd * max(dot(a, rd), 0.0) - a;
            let phi = atan2(dot(va, u2), dot(va, u1));
            var streak = 0.55 + 0.45 * noise1(phi * 9.0 + seed - time * 2.0);
            if (ty == 3) { streak = 0.25 + 1.2 * step(0.6, hash(floor(phi * 14.0) + floor(time * 30.0) + seed)); }
            if (ty == 9 || ty == 14) { streak = 0.35 + 0.9 * pow(0.5 + 0.5 * sin(phi * 5.0 + q.w * 9.0 - time * 10.0), 3.0); }
            let vv = vol * streak;
            core = vv * 0.25 * corek * mask * env;
            glow = vv * (glowk + 0.2) * 0.9 * mask * env;
            white = white * vv * 0.4;
        }
        if (shape == 1) {
            // A lunge always leaves a body-wide wake in its colour: the types drawn as specks,
            // spirals or helices (grass, bug, flying, dragon) were barely there on their own.
            let d0 = length(q.xyz - (a + ab * q.w));
            glow = glow + exp(-sq(d0 / max(r * 0.75, 0.1))) * 0.6 * env * along * inside;
            core = core + exp(-sq(d0 / max(r * 0.3, 0.05))) * 0.3 * env * along;
        }
        // The foam edge follows the mask too (a projectile's water trail left the whole line's outline).
        white = white * exp(-sq((d - w * 0.55) / max(w * 0.12, 0.01))) * 0.6 * env * along * min(mask, 1.0);
        // The dimming follows the mask too, or a ghost or dark projectile drew its whole line as a
        // black smear from the start.
        dark = exp(-sq(d / max(w * 0.9, 0.05))) * darkk * env * inside * min(mask, 1.0);
        if (shape == 6) { dark = darkk * env * vol * 0.6 * min(mask, 1.0); }
        if (orb < 0.0) {
            // Melee slash: a crescent swept diagonally across the reach's end, in the plane facing
            // the attacker (so it reads from behind, where the attacker sees it).
            let c = a + ab * 0.8;
            let den = dot(rd, dir);
            if (abs(den) > 0.05) {
                let tp = dot(c, dir) / den;
                if (tp > 0.0 && tp < tmax) {
                    let side = normalize(cross(dir, vec3<f32>(0.0, 1.0, 0.0)) + vec3<f32>(1e-4, 0.0, 0.0));
                    let upv = cross(side, dir);
                    let pp = rd * tp - c;
                    let flip = select(1.0, -1.0, fract(seed * 0.37) > 0.5);
                    let uv = rot2(vec2<f32>(dot(pp, side) * flip, dot(pp, upv)), 0.6) / max(r * 1.4, 0.5);
                    // The arc runs from 2.4 rad down to -0.6 rad as it sweeps, shaped by type.
                    let fade = 1.0 - smoothstep(0.35, 0.8, age);
                    if (length(uv) < 2.2) {
                        let sd = slash_dress(ty, uv, age, time, seed) * fade;
                        core = core + sd.x;
                        glow = glow + sd.y;
                        white = white + sd.z;
                        dark = max(dark, sd.w);
                        if (ty == 15) { col = vec3<f32>(0.9, 0.12, 0.15); }
                    }
                }
            }
        }
        if (orb > 0.0 && (fl & 16) == 0) {
            // Projectile orb: a glowing ball with a hot centre, gone shortly after it arrives,
            // dressed by type so a fireball, a water shot and a spark ball read apart.
            let oc = a + ab * orb;
            let hb = ray_ball(rd, tmax, oc, r * 1.3);
            let oe = 1.0 - smoothstep(0.2, 0.5, age);
            // Where on the orb's disc this pixel is (screen-facing), for rims and spikes.
            let otc = clamp(dot(oc, rd), 0.0, tmax);
            let ov = rd * otc - oc;
            let oang = atan2(dot(ov, u2), dot(ov, u1));
            var ocore = exp(-sq(hb.x / (r * 0.55))) * 1.2;
            // A tight halo: a long exp(-d) tail washed half the screen, cut off hard by the ground.
            var oglow = exp(-sq(hb.x / (r * 1.2))) * 0.75 + exp(-hb.x / (r * 0.6)) * 0.15;
            if (ty == 1) {
                // Fire: a ragged, flickering fireball whose flames trail back.
                let rag = 1.0 + 0.35 * noise1(oang * 3.0 + time * 18.0 + seed);
                oglow = exp(-sq(hb.x / (r * 1.25 * rag))) * 0.95 + exp(-hb.x / (r * 0.7 * rag)) * 0.2;
                col = mix(col, vec3<f32>(1.0, 0.25, 0.0), smoothstep(r * 0.3, r * 1.2, hb.x));
            } else if (ty == 2) {
                // Water: a bubble with a bright rim.
                white = white + ln(hb.x - r * 0.75, r * 0.1) * 0.7;
                ocore = ocore * 0.5;
            } else if (ty == 3) {
                // Electric: sparks crackle out of the ball.
                let sa = oang * 1.6 + 5.0;
                let spike = pow(hash(floor(sa) + floor(time * 30.0) + seed), 4.0) * ln(fract(sa) - 0.5, 0.08);
                oglow = oglow + spike * exp(-hb.x / (r * 1.3)) * 1.6;
            } else if (ty == 13 || ty == 15) {
                // Ghost, dark: a dim core that swallows light.
                dark = max(dark, exp(-sq(hb.x / (r * 0.8))) * 0.6 * hb.y * oe);
                ocore = ocore * 0.25;
            } else if (ty == 8 || ty == 12) {
                // Ground, rock: a solid, dull lump.
                ocore = (1.0 - smoothstep(r * 0.55, r * 0.65, hb.x)) * (0.3 + 0.12 * hash(floor(oang * 2.0) + seed));
                oglow = oglow * 0.25;
                col = col * 0.8;
            } else if (hb.x < r * 2.0) {
                // Every other type: its own dressing (leaf blade, crystal, air blade, ...).
                let side = normalize(cross(vec3<f32>(0.0, 1.0, 0.0), rd) + vec3<f32>(1e-4, 0.0, 0.0));
                let upv = cross(rd, side);
                let od = orb_dress(ty, vec2<f32>(dot(ov, side), dot(ov, upv)) / max(r, 0.1), vec2<f32>(dot(dir, side), dot(dir, upv)), time, seed);
                ocore = od.x;
                oglow = od.y;
                white = white + od.z * hb.y * oe;
                dark = max(dark, od.w * hb.y * oe);
            } else {
                ocore = 0.0;
                oglow = 0.0;
            }
            core = core + ocore * hb.y * oe;
            glow = glow + oglow * hb.y * oe;
        }
        if ((fl & 16) != 0 && shape == 0) {
            // Bullet: a solid spinning ball flies down the line, the line is just its trail.
            let hp = f.a.xyz + (f.b.xyz - f.a.xyz) * clamp(age * 2.5, 0.0, 1.0);
            let hb = ray_ball(rd, tmax, hp, r * 1.2);
            core = core * 0.35 + exp(-sq(hb.x / (r * 0.7))) * hb.y * env;
            glow = glow * 0.5 + exp(-hb.x / (r * 1.2)) * 0.5 * hb.y * env;
        }
    } else if (shape == 2 || shape == 7) {
        // Burst (Heat Wave): an expanding shell with a hot rim. Ground strike (7): the ring
        // crushes inward onto the point instead.
        let e = 1.0 - pow(1.0 - age, 3.0);
        var rr = r * (0.25 + 0.75 * e);
        if (shape == 7) { rr = r * (1.0 - 0.75 * e); }
        let hb = ray_ball(rd, tmax, f.a.xyz, rr);
        let env = 1.0 - smoothstep(0.5, 1.0, age);
        // The rim wobbles around the ball (by the direction from its centre, not the screen).
        let tc = clamp(dot(f.a.xyz, rd), 0.0, tmax);
        let out_dir = normalize(rd * tc - f.a.xyz + vec3<f32>(0.0, 1e-4, 0.0));
        let ang = atan2(out_dir.z, out_dir.x);
        var wobk = 0.08;
        var rimw = 0.1;
        var fillk = 0.06;
        var rimk = 0.55;
        if (ty == 1) { wobk = 0.16; }
        if (ty == 5 || ty == 16 || ty == 12) { wobk = 0.0; rimw = 0.04; }
        if (ty == 8) { fillk = 0.18; rimk = 0.2; }
        if (ty == 13 || ty == 15) { fillk = 0.0; dark = step(hb.x, rr) * 0.5 * env * hb.y; }
        let wob = 1.0 + wobk * noise1(ang * 4.0 + out_dir.y * 3.0 + time * 5.0 + seed);
        var rim = exp(-sq((hb.x - rr * wob) / max(rr * rimw, 0.06)));
        if (ty == 2) {
            // Water: a second ripple ring inside the first.
            rim = rim + 0.6 * exp(-sq((hb.x - rr * 0.6) / max(rr * 0.06, 0.05)));
        }
        if (ty == 3 || ty == 17) {
            // Electric crackle / fairy twinkle around the rim.
            rim = rim * (0.4 + 1.4 * pow(hash(floor(ang * 6.0) + floor(time * 20.0) + seed), 4.0));
        }
        if ((fl & (8 | 64)) != 0) {
            // Pulse / sound: several rings, not one.
            rim = rim + 0.7 * exp(-sq((hb.x - rr * fract(age * 3.0)) / max(rr * 0.06, 0.05)));
        }
        let fill = step(hb.x, rr) * fillk;
        glow = (rim * rimk + fill) * env * hb.y;
        core = rim * 0.25 * env * hb.y;
        // Footprint on the ground under it: the type's texture, and a signature's motif.
        if (tmax < 9000.0) {
            let p = rd * tmax;
            let dy = p.y - f.a.y;
            let uv = (p - f.a.xyz).xz / max(r, 0.5);
            // Only pixels on the footprint pay for the texture and motif.
            if (dy < 0.6 && dy > -r - 2.5 && dot(uv, uv) < 1.0 && env > 0.0) {
                let inside = 1.0 - smoothstep(0.85, 1.0, length(uv));
                let gt = ground_tex(ty, rot2(uv, seed), time, seed);
                let fade = env * inside * (1.0 - smoothstep(0.0, 2.5, -dy));
                glow = glow + gt.x * 0.35 * fade;
                dark = max(dark, gt.y * 0.6 * fade);
                if (mk > 0) {
                    mot = motif(mk, rot2(uv * 1.05, time * 0.3 + seed), time, seed) * fade;
                    if (grand) { mot = mot + ln(length(uv) - 0.98, 0.03) * fade; }
                }
            }
        }
    } else if (shape == 3) {
        // Hit flash: a quick bright puff, shaped by the move's flags and type.
        let hb = ray_ball(rd, tmax, f.a.xyz, r);
        let env = 1.0 - age;
        // A tight flash (the old exp(-d / r) halo washed a wide disc of the screen, cut off hard
        // where the ground in front hid the ball).
        core = exp(-sq(hb.x / (r * 0.3))) * env * env * hb.y * 0.6;
        glow = exp(-sq(hb.x / (r * 0.9))) * 0.35 * env * hb.y;
        let tc = clamp(dot(f.a.xyz, rd), 0.0, tmax);
        let v = rd * tc - f.a.xyz;
        // The type's own impact, drawn facing the camera (sparks, a splash, leaves, shards, ...).
        if (hb.x < r * 2.2) {
            let side = normalize(cross(vec3<f32>(0.0, 1.0, 0.0), rd) + vec3<f32>(1e-4, 0.0, 0.0));
            let upv = cross(rd, side);
            let hp = vec2<f32>(dot(v, side), dot(v, upv)) / max(r, 0.2);
            var k = 1.0;
            if ((fl & 7) != 0) { k = 0.6; }
            let hd = hit_dress(ty, hp, age, env, time, seed) * k * hb.y;
            if (ty == 15) { col = vec3<f32>(0.9, 0.12, 0.15); }
            core = core + hd.x;
            glow = glow + hd.y;
            white = white + hd.z;
            dark = max(dark, hd.w);
        }
        if ((fl & 1) != 0) {
            // Bite: upper and lower rows of fangs snap shut.
            let gap = r * 0.7 * (1.0 - smoothstep(0.0, 0.35, age));
            let tooth = 0.5 + 0.5 * cos((v.x + v.z) * 9.0);
            let arc = exp(-sq((abs(v.y) - gap) / (r * 0.08))) * step(length(v.xz), r);
            core = core * 0.4 + arc * tooth * env * 1.2 * hb.y;
        } else if ((fl & 2) != 0) {
            // Punch: a shock ring bursts outward (fighting draws its own in hit_dress).
            let rr = r * (0.3 + 1.2 * age);
            core = core + exp(-sq((length(v) - rr) / (r * 0.08))) * env * 1.1;
        } else if ((fl & 4) != 0) {
            // Slicing: a thin crescent cut across the target.
            let cut = abs(v.y + v.x * 0.6 - (age - 0.3) * r * 0.6);
            core = core * 0.3 + exp(-sq(cut / (r * 0.05))) * step(length(v), r * 1.3) * env * 1.4;
        }
        if (ty == 13 || ty == 15) { dark = max(dark, exp(-sq(hb.x / r)) * 0.35 * env * hb.y); }
        if (ty == 5) { core = core * (0.7 + 0.8 * pow(hash(floor(time * 25.0) + seed), 6.0)); }
    } else if (shape == 4) {
        // Guard: a faint bubble.
        let hb = ray_ball(rd, tmax, f.a.xyz, r);
        let rim = exp(-sq((hb.x - r) / 0.07));
        let shimmer = 0.75 + 0.25 * sin(time * 5.0 + hb.x * 9.0);
        glow = (rim * 0.5 + step(hb.x, r) * 0.04) * shimmer * hb.y;
    } else if (shape == 8) {
        // Wind-up: a ring of the type's colour closes in on the caster while a core gathers.
        let hb = ray_ball(rd, tmax, f.a.xyz, r);
        let g = age;
        let rr = r * (1.7 - 1.3 * g);
        let tc = clamp(dot(f.a.xyz, rd), 0.0, tmax);
        let v = rd * tc - f.a.xyz;
        let ang = atan2(v.z, v.x) + atan2(v.y, length(v.xz));
        var ring = exp(-sq((hb.x - rr) / max(r * 0.07, 0.035)));
        ring = ring * (0.45 + 0.9 * pow(hash(floor(ang * 5.0 + time * 7.0) + seed), 3.0));
        glow = (ring * 0.75 + exp(-sq(hb.x / (r * 0.55))) * 0.3 * g) * hb.y;
        core = exp(-sq(hb.x / (r * 0.2))) * g * g * 0.9 * hb.y;
        if (ty == 3) { glow = glow * (0.5 + hash(floor(time * 30.0) + seed)); }
        if (ty == 13 || ty == 15) { dark = exp(-sq(hb.x / r)) * 0.35 * g * hb.y; }
        if (mk > 0 && tmax < 9000.0) {
            // A signature's wind-up draws its motif on the ground, growing as it charges.
            let p = rd * tmax;
            let dy = p.y - f.a.y;
            if (dy < 0.3 && dy > -r * 2.5 - 1.0) {
                let uv = (p - f.a.xyz).xz / max(r * (0.9 + 0.9 * g), 0.4);
                mot = motif(mk, rot2(uv, time * 1.5), time, seed) * (0.3 + 0.7 * g);
            }
        }
    } else if (shape == 9) {
        // Ground warning (25% of the type's colour) where the scene is, in the hitbox's shape:
        // 1 line (width r), 2 cone (r wide at the far end), 3 circle (radius r). The filled part
        // grows toward the far end as the wind-up runs out.
        if (tmax > 9000.0) { return vec4<f32>(0.0); }
        let p = rd * tmax;
        let a = f.a.xyz;
        if (abs(p.y - a.y) > 1.8) { return vec4<f32>(0.0); }
        var inside = 0.0;
        var edge = 0.0;
        var prog = 0.0;
        if (fl == 3) {
            let dd = length((p - a).xz);
            inside = step(dd, r);
            edge = exp(-sq((dd - r) / 0.07));
            prog = step(dd, r * age);
        } else {
            let ab = (f.b.xyz - a).xz;
            let l = max(length(ab), 0.01);
            let u = ab / l;
            let v = (p - a).xz;
            let along = dot(v, u);
            let side = abs(v.x * u.y - v.y * u.x);
            var wid = r;
            if (fl == 2) { wid = r * clamp(along / l, 0.0, 1.0); }
            let span = step(0.0, along) * step(along, l);
            inside = span * step(side, wid);
            edge = span * exp(-sq((side - wid) / 0.07)) + step(side, wid) * exp(-sq((along - l) / 0.07));
            prog = inside * step(along, l * age);
        }
        let fade = clamp(age * 6.0, 0.0, 1.0);
        // (0.16 / 0.4 vanished on bright sand: additive colour needs more.)
        // Real opacity, not just added light: on bright sand or snow added colour barely shows,
        // so the warning also dims what's under it (alpha) and lays its colour on top.
        let pulse = (0.85 + 0.15 * sin(time * 12.0)) * fade;
        glow = (inside * 0.3 + prog * 0.2 + edge * 0.8) * pulse;
        dark = (inside * 0.25 + prog * 0.1 + edge * 0.4) * pulse;
    } else if (shape == 10) {
        // Zone: a signature's lingering area on the ground, in its type's texture, with a rim
        // and its motif turning slowly inside.
        if (tmax > 9000.0) { return vec4<f32>(0.0); }
        let p = rd * tmax;
        let a = f.a.xyz;
        if (abs(p.y - a.y) > 2.2) { return vec4<f32>(0.0); }
        let ab = (f.b.xyz - a).xz;
        let l = length(ab);
        var uv = (p - a).xz / max(r, 0.3);
        var inside = 0.0;
        var edge = 0.0;
        if (l < 0.05) {
            let dd = length(uv);
            inside = 1.0 - smoothstep(0.92, 1.0, dd);
            edge = ln(dd - 1.0, 0.05);
        } else {
            let u0 = ab / l;
            let v = (p - a).xz;
            let along = dot(v, u0);
            let side = (v.x * u0.y - v.y * u0.x) / max(r, 0.3);
            let span = step(0.0, along) * step(along, l);
            inside = span * (1.0 - smoothstep(0.9, 1.0, abs(side)));
            edge = span * ln(abs(side) - 1.0, 0.06);
            uv = vec2<f32>((fract(along / max(r * 2.0, 0.6)) - 0.5) * 2.0, side);
        }
        let env = smoothstep(0.0, 0.06, age) * (1.0 - smoothstep(0.82, 1.0, age));
        var gt = vec2<f32>(0.0);
        if (inside > 0.0) { gt = ground_tex(ty, uv, time, seed); }
        let pulse = 0.85 + 0.15 * sin(time * 6.0 + seed);
        glow = (inside * (0.12 + gt.x * 0.45) + edge * 0.6) * env * pulse;
        dark = inside * gt.y * 0.5 * env;
        core = edge * 0.2 * env;
        if (mk > 0 && inside > 0.0) {
            mot = motif(mk, rot2(uv, time * 0.4), time, seed) * inside * env * 0.8;
        }
    } else if (shape == 11) {
        // Aura: a status on a body. 1 stun (stars circling the head), 2 slow (rings sinking to
        // the feet), 3 ward (a hex shell), 4 empower (streaks rising), 5 charged (crackle),
        // 6 Ditto's morph (a wobbling purple goo shell that hides the body).
        let hb = ray_ball(rd, tmax, f.a.xyz, r * 1.2);
        let tc = clamp(dot(f.a.xyz, rd), 0.0, tmax);
        let v = rd * tc - f.a.xyz;
        // Angle round the body from the shell's front point (the closest-point vector lies in the
        // view plane, so its azimuth flips across the screen and left a hard seam).
        let ft = tc - sqrt(max(sq(r * 1.2) - dot(v, v), 0.0));
        let hp = rd * ft - f.a.xyz;
        let ang = atan2(hp.z, hp.x);
        // Screen-facing coordinates round the body (x across, y up, in body radii) and the angle
        // round its silhouette, for patterns that must stay smooth at the rim.
        let side = normalize(cross(vec3<f32>(0.0, 1.0, 0.0), rd) + vec3<f32>(1e-4, 0.0, 0.0));
        let sx = dot(v, side) / max(r, 0.3);
        let sy = v.y / max(r, 0.3);
        let s01 = fract(atan2(sy, sx) / 6.28318 + 1.0);
        if (fl == 1) {
            for (var i = 0; i < 3; i = i + 1) {
                let aa = time * 4.0 + f32(i) * 2.094;
                let sp = f.a.xyz + vec3<f32>(cos(aa) * r * 0.55, r * 1.05, sin(aa) * r * 0.55);
                let sb = ray_ball(rd, tmax, sp, 0.14);
                core = core + exp(-sq(sb.x / 0.07)) * sb.y * 1.4;
                glow = glow + exp(-sb.x / 0.18) * sb.y * 0.4;
            }
        } else if (fl == 2) {
            // Slow: blue rings tightening round the feet on the ground, and a cold tint on the body.
            glow = step(hb.x, r) * 0.06 * hb.y;
            dark = step(hb.x, r) * 0.08 * hb.y;
            if (tmax < 9000.0) {
                let p = rd * tmax;
                let dy = p.y - (f.a.y - r * 0.8);
                if (abs(dy) < 0.6) {
                    let dd = length((p - f.a.xyz).xz);
                    let k = fract(time * 0.8);
                    glow = glow + (ln(dd - r * (1.2 - 0.6 * k), 0.06) * (1.0 - k) + ln(dd - r * 0.55, 0.04) * 0.5) * 1.2;
                }
            }
        } else if (fl == 3) {
            let rim = ln(hb.x - r, 0.08);
            let hex = pow(abs(sin(ang * 6.0) * sin(v.y * 8.0 / max(r, 0.3))), 12.0);
            glow = (rim * 0.6 + step(hb.x, r) * (0.05 + 0.4 * hex)) * hb.y * (0.8 + 0.2 * sin(time * 4.0));
        } else if (fl == 4) {
            // Empower: golden streaks rising up the body in columns, and a warm rim.
            let cx = sx * 4.0;
            let cell = floor(cx);
            let col = ln(fract(cx) - 0.5, 0.12) * step(0.4, hash(cell + seed)) * (1.0 - smoothstep(0.8, 1.15, abs(sx)));
            let rise = pow(fract(sy * 0.9 - time * 2.2 + hash(cell * 1.7)), 4.0) * (1.0 - smoothstep(0.9, 1.4, abs(sy)));
            glow = (col * rise * 1.4 + ln(hb.x - r, 0.1) * 0.25) * hb.y;
        } else if (fl == 5) {
            let rim = ln(hb.x - r * 0.9, 0.06);
            glow = rim * (0.2 + 1.4 * pow(hash(floor(s01 * 31.0) + floor(time * 25.0) + seed), 6.0)) * hb.y;
        } else {
            let wob = 1.0 + 0.12 * noise1(ang * 3.0 + v.y * 4.0 + time * 7.0);
            let g = 1.0 - smoothstep(0.7, 1.0, age);
            let rim = ln(hb.x - r * wob, 0.12);
            glow = (rim * 0.8 + step(hb.x, r * wob) * 0.35) * g * hb.y;
            dark = step(hb.x, r * wob) * 0.45 * g * hb.y;
            mot = motif(22, v.xy / max(r, 0.3), time, seed) * step(hb.x, r * wob) * g * hb.y * 0.6;
        }
    } else if (shape == 5) {
        // Coat (Flame Charge): flickering fire around the body.
        let hb = ray_ball(rd, tmax, f.a.xyz, r);
        let flick = 0.6 + 0.4 * noise1(time * 14.0 + rd.x * 23.0 + rd.y * 41.0 + seed);
        let env = 1.0 - smoothstep(0.6, 1.0, age);
        glow = exp(-sq(hb.x / r) * 2.5) * 0.7 * flick * env * hb.y;
        core = exp(-sq(hb.x / (r * 0.5))) * 0.25 * flick * env * hb.y;
    }
    let s = f.color.a;
    var mk2 = mot * 1.6;
    if (grand) { mk2 = mk2 * 1.35; }
    // The type-coloured light is capped so its brightest channel stays near 1.3: past that every
    // channel clips to white on screen and ice, steel, rock and pale beams all looked the same.
    // Scaling all channels together keeps the hue; deliberate white (foam, beam cores) is added after.
    // Pale type colours (ice, flying, steel, fairy) are pushed a little more saturated for the same reason.
    let lum = dot(col, vec3<f32>(0.299, 0.587, 0.114));
    let cs = max(mix(vec3<f32>(lum), col, 1.45), vec3<f32>(0.0));
    var tinted = cs * glow * 1.2 + mix(cs, vec3<f32>(1.0), 0.25) * core * 1.8;
    let peak = max(tinted.r, max(tinted.g, tinted.b));
    tinted = tinted * min(1.0, 1.15 / max(peak, 1e-4));
    let light = (tinted + vec3<f32>(1.0) * white + f.m.rgb * mk2) * s;
    return vec4<f32>(light, clamp(dark * s, 0.0, 0.85));
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let size = u.screen.xy;
    let dsize = u.screen.zw;
    let uv = pos.xy / size;
    let dc = vec2<i32>(clamp(floor(pos.xy * dsize / size), vec2<f32>(0.0), dsize - 1.0));
    let d = textureLoad(depth_tex, dc, 0);
    let ndc = vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    // The eye isn't at the origin while view bobbing sways the camera (see `eye_offset`).
    let eye = u.eye.xyz;
    let rd = normalize(unproject(vec3<f32>(ndc, 0.5)) - eye);
    var tmax = 100000.0;
    if (d > 0.0) {
        tmax = length(unproject(vec3<f32>(ndc, d)) - eye);
    }
    let n = i32(u.info.x);
    let time = u.info.y;
    var col = vec3<f32>(0.0);
    var keep = 1.0;
    for (var i = 0; i < n; i = i + 1) {
        let o = one(u.fx[i], rd, tmax, time);
        col = col + o.rgb;
        keep = keep * (1.0 - o.a);
    }
    // Blend: light adds, and alpha (1 - keep) dims the scene behind (ghost and dark only).
    return vec4<f32>(min(col, vec3<f32>(6.0)), 1.0 - keep);
}
"#;

const UNIFORM_FLOATS: usize = 16 + 4 + 4 + 4 + MAX_FX * FX_FLOATS;

/// Where the eye really is, relative to `WorldDrawn::camera`: the matrix carries view bobbing's
/// sway (`frame.bob`), so the eye is off the origin while walking. It's the point the matrix
/// sends to clip x = y = w = 0. Zero if the matrix is degenerate.
fn eye_offset(m: Mat4) -> DVec3 {
    let c = |i: usize| m.col(i).as_dvec4();
    let (c0, c1, c2, c3) = (c(0), c(1), c(2), c(3));
    let rows = DMat3::from_cols(
        DVec3::new(c0.x, c0.y, c0.w),
        DVec3::new(c1.x, c1.y, c1.w),
        DVec3::new(c2.x, c2.y, c2.w),
    );
    if rows.determinant().abs() < 1e-12 {
        return DVec3::ZERO;
    }
    let eye = rows.inverse() * -DVec3::new(c3.x, c3.y, c3.w);
    if eye.is_finite() && eye.length() < 4.0 { eye } else { DVec3::ZERO }
}

struct Gpu {
    format: wgpu::TextureFormat,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    uniforms: wgpu::Buffer,
}

/// The render side, handed to Horizon by `Hooks::render`.
pub struct Render {
    gpu: Mutex<Option<Gpu>>,
    off: AtomicBool,
    told: AtomicBool,
}

pub static RENDER: LazyLock<Render> =
    LazyLock::new(|| Render { gpu: Mutex::new(None), off: AtomicBool::new(false), told: AtomicBool::new(false) });

/// Poll a future once: wgpu's error scopes resolve immediately on native backends.
fn poll_now<F: std::future::Future>(f: F) -> Option<F::Output> {
    let mut f = std::pin::pin!(f);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match f.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(v) => Some(v),
        std::task::Poll::Pending => None,
    }
}

fn build(device: &wgpu::Device, format: wgpu::TextureFormat) -> Result<Gpu, String> {
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("capipimon fx"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("capipimon fx"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Depth,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("capipimon fx"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    // Light adds; a ghost or dark effect's alpha also dims what is behind it (0 for the rest,
    // so they stay purely additive).
    let add = wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
        operation: wgpu::BlendOperation::Add,
    };
    let keep = wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::Zero,
        dst_factor: wgpu::BlendFactor::One,
        operation: wgpu::BlendOperation::Add,
    };
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("capipimon fx"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState { color: add, alpha: keep }),
                write_mask: wgpu::ColorWrites::COLOR,
            })],
        }),
        multiview_mask: None,
        cache: None,
    });
    let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("capipimon fx"),
        size: (UNIFORM_FLOATS * 4) as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    match poll_now(scope.pop()) {
        Some(Some(e)) => Err(e.to_string()),
        Some(None) => Ok(Gpu { format, pipeline, layout, uniforms }),
        None => Err("error scope didn't resolve".into()),
    }
}

impl Render {
    fn switch_off(&self, why: &str) {
        self.off.store(true, Ordering::Relaxed);
        log::warn!("capipimon_models: move effects pass switched off ({why}); particles still show");
    }
}

impl RenderHooks for Render {
    fn render_needs(&self, event: &mut RenderNeedsEvent<'_>) {
        if !self.off.load(Ordering::Relaxed) && FX.lock().map(|f| !f.low && (!f.effects.is_empty() || !f.guarding.is_empty())).unwrap_or(false) {
            event.needs.world_depth = true;
        }
    }

    fn world_drawn(&self, event: &mut WorldDrawnEvent<'_, '_>) {
        if self.off.load(Ordering::Relaxed) {
            return;
        }
        let w = &mut *event.world;
        let eye = eye_offset(w.view_proj);
        let list = snapshot(w.camera + eye);
        if list.is_empty() {
            return;
        }
        if !self.told.swap(true, Ordering::Relaxed) {
            log::info!(
                "capipimon_models: move effects pass: format {:?}, size {:?}, depth {:?}, samples {}",
                w.color_format,
                w.size,
                w.depth_size,
                w.color.sample_count()
            );
        }
        if w.color.sample_count() != 1 || !w.color.usage().contains(wgpu::TextureUsages::RENDER_ATTACHMENT) {
            return self.switch_off("frame target isn't a plain render attachment");
        }
        let Ok(mut gpu) = self.gpu.lock() else { return };
        if gpu.as_ref().map(|g| g.format) != Some(w.color_format) {
            match build(w.device, w.color_format) {
                Ok(g) => *gpu = Some(g),
                Err(e) => return self.switch_off(&format!("pipeline rejected: {e}")),
            }
        }
        let Some(g) = gpu.as_ref() else { return };
        let mut floats = Vec::with_capacity(UNIFORM_FLOATS);
        floats.extend_from_slice(&w.jittered.inverse().to_cols_array());
        floats.extend_from_slice(&[w.size[0] as f32, w.size[1] as f32, w.depth_size[0] as f32, w.depth_size[1] as f32]);
        let count = list.len() / FX_FLOATS;
        floats.extend_from_slice(&[count as f32, EPOCH.elapsed().as_secs_f32() % 1000.0, 0.0, 0.0]);
        floats.extend_from_slice(&[eye.x as f32, eye.y as f32, eye.z as f32, 0.0]);
        floats.extend_from_slice(&list);
        floats.resize(UNIFORM_FLOATS, 0.0);
        let bytes: Vec<u8> = floats.iter().flat_map(|f| f.to_le_bytes()).collect();
        w.queue.write_buffer(&g.uniforms, 0, &bytes);
        let scope = w.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let bind = w.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("capipimon fx"),
            layout: &g.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: g.uniforms.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(w.depth) },
            ],
        });
        match poll_now(scope.pop()) {
            Some(None) => {}
            Some(Some(e)) => return self.switch_off(&format!("depth binding rejected: {e}")),
            None => return self.switch_off("error scope didn't resolve"),
        }
        let mut pass = w.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("capipimon fx"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: w.color_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&g.pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.draw(0..3, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eye_offset_finds_a_swayed_eye() {
        let proj = glam::Mat4::perspective_infinite_reverse_rh(1.2, 1.6, 0.05);
        let view = glam::Mat4::from_rotation_y(0.3) * glam::Mat4::from_translation(glam::Vec3::new(-0.08, 0.12, 0.03));
        let eye = super::eye_offset(proj * view);
        assert!((eye - glam::DVec3::new(0.08, -0.12, -0.03)).length() < 1e-4, "{eye:?}");
        assert!(super::eye_offset(proj).length() < 1e-6);
    }

    #[test]
    fn parses_fx_and_hit() {
        assert!(heard(&["fx", "thunderbolt", "electric", "beam", "1,2,3", "4,5,6", "0.40", "bob", "77", "0.45"], None));
        assert!(heard(&["hit", "alice", "4,5,6", "electric", "12"], Some("alice")));
        assert!(heard(&["guard", "alice", "1"], None));
        let fx = FX.lock().unwrap();
        assert!(fx.effects.iter().any(|e| e.shape == Shape::Beam && e.style == 3 && e.b == DVec3::new(4.0, 5.0, 6.0)));
        assert!(fx.effects.iter().any(|e| e.shape == Shape::Hit));
        assert!(fx.guarding.contains(&"alice".to_string()));
        assert!(trauma_now(&fx) > 0.0);
    }

    #[test]
    fn archetype_looks_skip_moves_with_their_own_line_look() {
        assert_eq!(arch_index("ancientpower"), 1);
        assert_eq!(arch_index("acidspray"), 0, "pulse/bullet/wind/sound keep their look");
        assert_eq!(arch_index("aerialace"), 2);
        assert_eq!(arch_index("sig:pikachu"), 0);
    }

    #[test]
    fn meteor_gets_a_comet_and_blink_gets_poofs() {
        assert!(heard(&["fx", "sig:amaura", "rock", "ground", "5,0,5", "5,0,5", "2.0", "met", "3", "0.5"], None));
        assert!(heard(&["fx", "sig:accelgor", "dark", "dash", "0,1,0", "4,1,0", "0.5", "met", "4", "0.3"], None));
        let fx = FX.lock().unwrap();
        assert!(fx.effects.iter().any(|e| e.caster == "met" && e.shape == Shape::Dash && e.a.y > 10.0 && e.b == DVec3::new(5.0, 0.0, 5.0)));
        assert_eq!(fx.effects.iter().filter(|e| e.caster == "met" && e.shape == Shape::Hit && e.radius == 1.0).count(), 2);
    }

    #[test]
    fn demo_rounds_show_six_types_per_page() {
        let now = Instant::now();
        for what in ["hit", "orb", "orbside", "melee", "beam", "lunge", "cone", "strike", "burst"] {
            for page in 0..3 {
                let round = demo_effects(what, page, DVec3::ZERO, now);
                assert_eq!(round.len(), 6, "{what} page {page}");
                assert_eq!(round[0].style as usize, page * 6, "{what} page {page} starts at its first type");
            }
        }
        assert!(demo_effects("off", 0, DVec3::ZERO, now).is_empty());
        // Archetype looks ride in look[3] / 64, like a kit move's (`heard`).
        assert_eq!(demo_effects("melee", 0, DVec3::ZERO, now)[0].look[3], 128.0);
    }

    #[test]
    fn signature_zone_carries_its_look() {
        assert!(heard(&["fx", "sig:pikachu", "electric", "zone", "1,2,3", "1,2,3", "2.0", "zed", "5", "3.0"], None));
        assert!(heard(&["fx", "transform", "normal", "beam", "0,0,0", "0,0,8", "0.3", "zed", "6", "0.4"], None));
        assert!(heard(&["fx", "sig:pikachu", "electric", "spiral", "1,2,3", "1,2,3", "2.0", "zed", "5", "3.0"], None), "unknown shapes are eaten, not drawn");
        let fx = FX.lock().unwrap();
        let z = fx.effects.iter().find(|e| e.shape == Shape::Zone && e.caster == "zed").expect("zone");
        assert!(z.look != [0.0; 4], "a signature zone draws its motif");
        assert!(fx.effects.iter().any(|e| e.shape == Shape::Beam && e.caster == "zed" && e.look != [0.0; 4]));
    }
}
