//! The trainer kit as everyone sees it (GDD 6.1/6.2): a thrown Poké Ball on its arc, the capture
//! (the ball opens, pulls its target in as light, shuts, hangs in the air and wobbles while the
//! target struggles, then clicks shut or bursts open), the Tether Rope, the faint line a trainer
//! sees while aiming a throw, and a Potion's green glow. Drawn by the effects pass (`fx.rs`).
//!
//! Wire, server to client (new lines an older client ignores):
//! - `tball <id> fly <from> <to|@anchor> <secs> <hit 0|1>`: a ball leaves a hand. An anchor is a
//!   player name or `e:<entity id>`; the ball homes on that body.
//! - `tball <id> hold <@anchor|-> <at> <secs>`: the capture starts where the target stands.
//! - `tball <id> out <caught 0|1>`: how it ended.
//! - `rope <id> <@from> <@to> <secs>` / `rope <id> -`.
//! - `heal <@anchor>`.
//! - `mark <id> <@anchor|point> <kind 0-3> <secs>` / `mark <id> -`: a trainer's order over its
//!   target (0 a bullseye, 1 a Z, 2 both, 3 a spot on the ground). One per id.
//! - `aimline <from> <to> <secs>`: a throw someone else is lining up (the tutorial's launcher).

use super::{Effect, Fx, Shape};
use glam::{DVec3, Vec3};
use std::time::Instant;

/// The ball's radius (blocks).
const BALL_R: f32 = 0.26;
const BALL_RED: Vec3 = Vec3::new(0.93, 0.16, 0.1);
/// The capture's opening act, before the wobbling starts (s): up and open, pull in, shut, settle.
const OPEN_SECS: f32 = 1.0;
/// A missed ball's bounce (s), a caught ball's click and stars, a broken ball's burst.
const MISS_SECS: f32 = 0.55;
const CAUGHT_SECS: f32 = 1.1;
const BREAK_SECS: f32 = 0.6;
/// One wobble cycle (s): rocks for the first part, rests for the rest.
const WOBBLE_SECS: f32 = 1.3;
const WOBBLE_ROCK: f32 = 0.55;
/// How high over the ground the ball hangs while it wobbles, and how high it pops to open.
const HOVER: f64 = 1.15;

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Anchor {
    Point(DVec3),
    /// A player name or `e:<entity id>`: wherever that body is now.
    Body(String),
}

impl Anchor {
    fn parse(s: &str) -> Option<Anchor> {
        match s.strip_prefix('@') {
            Some(name) if !name.is_empty() => Some(Anchor::Body(name.to_string())),
            Some(_) => None,
            None => super::parse_v3(s).map(Anchor::Point),
        }
    }

    fn name(&self) -> Option<&str> {
        match self {
            Anchor::Body(n) => Some(n),
            Anchor::Point(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    /// A trainer's throw: the ball is on the snowball `to` names, wherever that flies.
    Thrown,
    Fly { hit: bool },
    /// Landed on its target, waiting for the server's `hold`.
    Landed,
    Miss,
    Hold,
    Caught,
    Broke,
    /// A treat tossed to a Pokémon: flies like a ball, smaller, and is gone when it lands.
    Treat,
}

#[derive(Clone, Debug)]
struct Ball {
    id: String,
    phase: Phase,
    from: DVec3,
    to: Anchor,
    /// Where it was last drawn (a body that has gone keeps its last place).
    at: DVec3,
    start: Instant,
    secs: f32,
    /// Sounds of this phase already played.
    played: u32,
    seed: u32,
}

#[derive(Clone, Debug)]
struct Rope {
    id: String,
    from: String,
    to: String,
    start: Instant,
    secs: f32,
}

#[derive(Clone, Debug)]
struct Mark {
    id: String,
    at: Anchor,
    kind: u32,
    start: Instant,
    secs: f32,
}

#[derive(Default)]
pub(super) struct State {
    balls: Vec<Ball>,
    ropes: Vec<Rope>,
    /// The throw a trainer is lining up: hand, landing spot, whether a Pokémon is under it, and
    /// how high the arc rises.
    aim: Option<(DVec3, DVec3, bool, f64)>,
    /// Throws the server shows coming: hand, landing spot, since, for how long (s).
    lines: Vec<(DVec3, DVec3, Instant, f32)>,
    marks: Vec<Mark>,
}

/// How tall each NPC Pokémon's model is, by entity id (the engine's height is its carrier's).
static NPC_HEIGHTS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<u32, f32>>> = std::sync::LazyLock::new(Default::default);

/// An NPC Pokémon was drawn: its model's height, for the effects that sit on it.
pub fn note_npc(id: u32, height: f32) {
    let mut m = NPC_HEIGHTS.lock().unwrap();
    if m.len() > 512 {
        m.clear();
    }
    m.insert(id, height);
}

pub fn npc_height(id: u32) -> Option<f32> {
    NPC_HEIGHTS.lock().unwrap().get(&id).copied()
}

/// The hand a throw leaves: shoulder height, a little to the right of the body.
pub fn hand(feet: [f64; 3], yaw: f32) -> [f64; 3] {
    let y = f64::from(yaw).to_radians();
    [feet[0] - y.cos() * 0.35, feet[1] + 1.35, feet[2] - y.sin() * 0.35]
}

/// A thrown ball is a vanilla snowball. These are a snowball's own numbers, for the aim line:
/// blocks a tick (a twentieth of a second) as it leaves, what it loses to the air each tick,
/// and how much faster it falls each tick.
pub const THROW_SPEED: f64 = 1.5;
const THROW_DRAG: f64 = 0.99;
const THROW_GRAVITY: f64 = 0.03;
/// A thrown ball that has met nothing is gone after this many ticks.
pub const THROW_MAX_TICKS: u32 = 100;

/// Where a throw leaves from, as vanilla throws: just under the eye.
pub fn throw_from(eye: [f64; 3]) -> [f64; 3] {
    [eye[0], eye[1] - 0.1, eye[2]]
}

/// How a throw along the look `d` leaves, in blocks a tick.
pub fn throw_velocity(d: [f64; 3]) -> [f64; 3] {
    d.map(|c| c * THROW_SPEED)
}

/// Where a thrown ball would stop, after how long (s), the body it would fly into (an index),
/// and how high its path rises over the straight line there.
pub struct Throw {
    pub end: [f64; 3],
    pub secs: f64,
    pub hit: Option<usize>,
    pub arc: f64,
}

/// The path a snowball takes from `from` at `vel` (blocks a tick), to the first body (feet and
/// height) or solid thing in its way. `clear`: whether nothing solid lies between two points.
/// Only the aim line asks: the thrown ball itself is the game's own snowball.
pub fn throw_path(from: [f64; 3], vel: [f64; 3], bodies: &[([f64; 3], f64)], clear: &mut dyn FnMut([f64; 3], [f64; 3]) -> bool) -> Throw {
    let inside = |q: DVec3| {
        bodies.iter().position(|(feet, h)| (q.x - feet[0]).hypot(q.z - feet[2]) < (h * 0.35).max(0.45) + 0.3 && q.y > feet[1] - 0.3 && q.y < feet[1] + h + 0.3)
    };
    let (start, mut a, mut v) = (DVec3::from(from), DVec3::from(from), DVec3::from(vel));
    let mut path = vec![a];
    let mut stop: Option<(DVec3, f64, Option<usize>)> = None;
    for tick in 0..THROW_MAX_TICKS {
        let b = a + v;
        // How far into this tick's step the way is clear.
        let mut reach = 1.0;
        let blocked = !clear(a.to_array(), b.to_array());
        if blocked {
            let (mut lo, mut hi) = (0.0, 1.0);
            for _ in 0..6 {
                let mid = (lo + hi) * 0.5;
                if clear(a.to_array(), a.lerp(b, mid).to_array()) {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            reach = lo;
        }
        let body = (1..=8).map(|i| f64::from(i) / 8.0).take_while(|s| *s <= reach).find_map(|s| inside(a.lerp(b, s)).map(|k| (s, k)));
        if let Some((s, k)) = body {
            stop = Some((a.lerp(b, s), (f64::from(tick) + s) / 20.0, Some(k)));
        } else if blocked {
            stop = Some((a.lerp(b, reach), (f64::from(tick) + reach) / 20.0, None));
        }
        if stop.is_some() {
            break;
        }
        path.push(b);
        a = b;
        v *= THROW_DRAG;
        v.y -= THROW_GRAVITY;
    }
    let (end, secs, hit) = stop.unwrap_or((a, f64::from(THROW_MAX_TICKS) / 20.0, None));
    // The drawn arc goes through the path's middle.
    let mid = path[path.len() / 2];
    let arc = if path.len() > 2 { (mid.y - (start.y + end.y) * 0.5).max(0.0) } else { 0.0 };
    Throw { end: end.to_array(), secs, hit, arc }
}

/// How high a throw's arc rises over the straight line, by its length.
pub fn arc_height(dist: f64) -> f64 {
    (dist * 0.16).clamp(0.4, 2.4)
}

/// A point `s` (0-1) along the throw from `a` to `b`.
pub fn arc_at(a: DVec3, b: DVec3, s: f64) -> DVec3 {
    a.lerp(b, s) + DVec3::Y * arc_height(a.distance(b)) * 4.0 * s * (1.0 - s)
}

/// How long a throw of this length flies (s). The server delays the capture by the same.
pub fn flight_secs(dist: f64) -> f32 {
    (dist / 24.0).clamp(0.25, 0.75) as f32
}

/// The ball's lid (0 shut to 1 open), and how far it has come from the target's chest to its
/// hover point (0-1), `t` seconds into the capture's opening act.
fn opening(t: f32) -> (f32, f32) {
    let rise = (t / 0.2).clamp(0.0, 1.0);
    let lid = if t < 0.2 { t / 0.2 } else if t < 0.62 { 1.0 } else { (1.0 - (t - 0.62) / 0.13).max(0.0) };
    (lid, 1.0 - (1.0 - rise).powi(2))
}

/// The wobble `t` seconds into the hold (after the opening act): the tilt in radians and how
/// brightly the button glows.
fn wobble(t: f32) -> (f32, f32) {
    let u = t.rem_euclid(WOBBLE_SECS);
    if u >= WOBBLE_ROCK {
        return (0.0, 0.0);
    }
    let k = u / WOBBLE_ROCK;
    let env = (std::f32::consts::PI * k).sin();
    ((k * std::f32::consts::TAU * 2.0).sin() * 0.5 * env, env)
}

pub(super) fn heard(fx: &mut Fx, f: &[&str]) -> bool {
    match f {
        // A trainer's thrown ball is a snowball (entity `ent`): the ball is drawn where it flies.
        ["tball", id, "shot", ent, ..] => {
            let Some(to) = Anchor::parse(ent) else { return true };
            let t = &mut fx.trainer;
            t.balls.retain(|b| b.id != *id);
            t.balls.push(Ball { id: id.to_string(), phase: Phase::Thrown, from: DVec3::NAN, to, at: DVec3::NAN, start: Instant::now(), secs: THROW_MAX_TICKS as f32 / 20.0 + 1.0, played: 0, seed: 7 });
            if t.balls.len() > 12 {
                t.balls.remove(0);
            }
            true
        }
        // It met a block, or a Pokémon it can't take: one hop at `at` (or where it was last seen).
        ["tball", id, "miss", rest @ ..] => {
            if let Some(b) = fx.trainer.balls.iter_mut().find(|b| b.id == *id) {
                if let Some(at) = rest.first().and_then(|p| super::parse_v3(p)) {
                    b.at = at;
                }
                b.phase = Phase::Miss;
                b.secs = MISS_SECS;
                b.start = Instant::now();
                b.played = 0;
            }
            fx.trainer.balls.retain(|b| !b.at.is_nan());
            true
        }
        ["tball", id, "fly", from, to, secs, hit, ..] => {
            let (Some(from), Some(to)) = (super::parse_v3(from), Anchor::parse(to)) else { return true };
            let secs = secs.parse::<f32>().unwrap_or(0.4).clamp(0.1, 2.0);
            let t = &mut fx.trainer;
            t.balls.retain(|b| b.id != *id);
            t.balls.push(Ball { id: id.to_string(), phase: Phase::Fly { hit: *hit == "1" }, from, to, at: from, start: Instant::now(), secs, played: 0, seed: (from.x * 31.0 + from.z * 17.0).abs() as u32 % 997 });
            if t.balls.len() > 12 {
                t.balls.remove(0);
            }
            true
        }
        ["treat", id, from, to, secs, ..] => {
            let (Some(from), Some(to)) = (super::parse_v3(from), Anchor::parse(to)) else { return true };
            let secs = secs.parse::<f32>().unwrap_or(0.4).clamp(0.1, 2.0);
            let t = &mut fx.trainer;
            t.balls.push(Ball { id: id.to_string(), phase: Phase::Treat, from, to, at: from, start: Instant::now(), secs, played: 0, seed: 7 });
            if t.balls.len() > 12 {
                t.balls.remove(0);
            }
            true
        }
        ["tball", id, "hold", anchor, at, secs, ..] => {
            let Some(at) = super::parse_v3(at) else { return true };
            let secs = secs.parse::<f32>().unwrap_or(6.0).clamp(0.5, 30.0);
            let to = Anchor::parse(anchor).unwrap_or(Anchor::Point(at));
            let t = &mut fx.trainer;
            let from = t.balls.iter().find(|b| b.id == *id).map(|b| b.at).filter(|p| !p.is_nan()).unwrap_or(at + DVec3::Y);
            t.balls.retain(|b| b.id != *id);
            // `from` is where the target stands; `at` starts at the ball's own place.
            t.balls.push(Ball { id: id.to_string(), phase: Phase::Hold, from: at, to, at: from, start: Instant::now(), secs: secs + OPEN_SECS, played: 0, seed: 7 });
            true
        }
        ["tball", id, "out", caught, ..] => {
            if let Some(b) = fx.trainer.balls.iter_mut().find(|b| b.id == *id) {
                let caught = *caught == "1";
                b.phase = if caught { Phase::Caught } else { Phase::Broke };
                b.secs = if caught { CAUGHT_SECS } else { BREAK_SECS };
                b.start = Instant::now();
                b.played = 0;
            }
            true
        }
        ["rope", id, "-", ..] => {
            fx.trainer.ropes.retain(|r| r.id != *id);
            true
        }
        ["rope", id, from, to, secs, ..] => {
            let (Some(from), Some(to)) = (from.strip_prefix('@'), to.strip_prefix('@')) else { return true };
            let secs = secs.parse::<f32>().unwrap_or(10.0).clamp(0.5, 120.0);
            fx.trainer.ropes.retain(|r| r.id != *id);
            fx.trainer.ropes.push(Rope { id: id.to_string(), from: from.to_string(), to: to.to_string(), start: Instant::now(), secs });
            if fx.sounds.len() < 8 {
                fx.sounds.push(("minecraft:item.lead.tied", Err(to.to_string()), 1.0, 0.9));
            }
            true
        }
        ["mark", id, "-", ..] => {
            fx.trainer.marks.retain(|m| m.id != *id);
            true
        }
        ["mark", id, at, kind, secs, ..] => {
            let Some(at) = Anchor::parse(at) else { return true };
            let secs = secs.parse::<f32>().unwrap_or(6.0).clamp(0.5, 60.0);
            fx.trainer.marks.retain(|m| m.id != *id);
            fx.trainer.marks.push(Mark { id: id.to_string(), at, kind: kind.parse::<u32>().unwrap_or(0).min(3), start: Instant::now(), secs });
            if fx.trainer.marks.len() > 8 {
                fx.trainer.marks.remove(0);
            }
            true
        }
        ["aimline", from, to, secs, ..] => {
            let (Some(from), Some(to)) = (super::parse_v3(from), super::parse_v3(to)) else { return true };
            let secs = secs.parse::<f32>().unwrap_or(1.0).clamp(0.1, 10.0);
            fx.trainer.lines.push((from, to, Instant::now(), secs));
            if fx.trainer.lines.len() > 4 {
                fx.trainer.lines.remove(0);
            }
            true
        }
        ["theal", who, ..] => {
            let Some(who) = who.strip_prefix('@') else { return true };
            fx.effects.push(Effect {
                shape: Shape::Aura,
                a: DVec3::ZERO,
                b: DVec3::ZERO,
                radius: 1.0,
                rgb: Vec3::new(0.3, 1.0, 0.45),
                seed: 11,
                start: Instant::now(),
                secs: 1.8,
                caster: who.to_string(),
                strength: 1.0,
                style: 0,
                flags: super::AURA_HEAL,
                look: [0.0; 4],
            });
            if fx.sounds.len() < 7 {
                fx.sounds.push(("minecraft:block.amethyst_block.chime", Err(who.to_string()), 1.0, 1.3));
                fx.sounds.push(("minecraft:entity.generic.drink", Err(who.to_string()), 0.8, 1.2));
            }
            true
        }
        _ => false,
    }
}

/// The throw a trainer is lining up (None: not aiming).
pub(super) fn set_aim(fx: &mut Fx, aim: Option<(DVec3, DVec3, bool, f64)>) {
    fx.trainer.aim = aim;
}

/// The bodies the kit's effects need this frame, by name.
pub(super) fn follows(fx: &Fx) -> Vec<String> {
    let t = &fx.trainer;
    t.balls
        .iter()
        .filter_map(|b| b.to.name().map(str::to_string))
        .chain(t.ropes.iter().flat_map(|r| [r.from.clone(), r.to.clone()]))
        .chain(t.marks.iter().filter_map(|m| m.at.name().map(str::to_string)))
        .collect()
}

/// Once a frame, before the bodies are looked up: phases that have run out move on, and each
/// phase's sounds play on time.
pub(super) fn frame(fx: &mut Fx) {
    let mut sounds: Vec<(&'static str, DVec3, f32, f32)> = Vec::new();
    let t = &mut fx.trainer;
    t.ropes.retain(|r| r.start.elapsed().as_secs_f32() < r.secs);
    t.lines.retain(|l| l.2.elapsed().as_secs_f32() < l.3);
    t.marks.retain(|m| m.start.elapsed().as_secs_f32() < m.secs);
    for b in &mut t.balls {
        let age = b.start.elapsed().as_secs_f32();
        let mut say = |n: u32, when: f32, sound: &'static str, vol: f32, pitch: f32, played: &mut u32| {
            if *played == n && age >= when {
                *played += 1;
                sounds.push((sound, b.at, vol, pitch));
            }
        };
        match b.phase {
            Phase::Fly { hit } => {
                say(0, 0.0, "minecraft:entity.snowball.throw", 0.9, 0.7, &mut b.played);
                if age >= b.secs {
                    b.phase = if hit { Phase::Landed } else { Phase::Miss };
                    b.secs = if hit { 0.6 } else { MISS_SECS };
                    b.start = Instant::now();
                    b.played = 0;
                }
            }
            Phase::Thrown => say(0, 0.0, "minecraft:entity.snowball.throw", 0.9, 0.7, &mut b.played),
            Phase::Miss => say(0, 0.0, "minecraft:block.stone_button.click_off", 0.8, 0.8, &mut b.played),
            Phase::Landed => {}
            Phase::Hold => {
                say(0, 0.0, "minecraft:block.iron_trapdoor.open", 0.9, 1.7, &mut b.played);
                say(1, 0.12, "minecraft:block.beacon.deactivate", 0.8, 1.9, &mut b.played);
                say(2, 0.66, "minecraft:block.iron_trapdoor.close", 0.9, 1.7, &mut b.played);
                // One tick of the catch for each wobble.
                let n = ((age - OPEN_SECS) / WOBBLE_SECS).floor();
                if age >= OPEN_SECS && b.played < 3 + n as u32 + 1 {
                    b.played = 3 + n as u32 + 1;
                    sounds.push(("minecraft:block.note_block.hat", b.at, 0.9, 0.7));
                }
            }
            Phase::Caught => {
                say(0, 0.0, "minecraft:block.iron_door.close", 0.9, 1.9, &mut b.played);
                say(1, 0.12, "minecraft:block.amethyst_block.chime", 1.0, 1.6, &mut b.played);
            }
            Phase::Broke => say(0, 0.0, "minecraft:block.iron_trapdoor.open", 1.0, 1.2, &mut b.played),
            Phase::Treat => say(0, 0.0, "minecraft:entity.snowball.throw", 0.7, 1.3, &mut b.played),
        }
    }
    // A hold outlives its clock by a little: the server's `out` ends it, this only cleans up.
    t.balls.retain(|b| b.start.elapsed().as_secs_f32() < b.secs + if b.phase == Phase::Hold { 3.0 } else { 0.0 });
    for (sound, at, vol, pitch) in sounds {
        if fx.sounds.len() < 12 {
            fx.sounds.push((sound, Ok(at), vol, pitch));
        }
    }
}

fn ball_effect(at: DVec3, other: DVec3, phase: u32, age: f32, secs: f32, look: [f32; 4], strength: f32, seed: u32) -> Effect {
    Effect {
        shape: Shape::Ball,
        a: at,
        b: other,
        radius: BALL_R,
        rgb: BALL_RED,
        seed,
        start: Instant::now().checked_sub(std::time::Duration::from_secs_f32((age * secs).max(0.0))).unwrap_or_else(Instant::now),
        secs,
        caster: String::new(),
        strength,
        style: 0,
        flags: phase,
        look,
    }
}

/// What to draw this frame. `body`: where a named body's feet are, and how tall it is.
pub(super) fn effects(fx: &mut Fx, body: &dyn Fn(&str) -> Option<(DVec3, f64)>) -> Vec<Effect> {
    let mut out = Vec::new();
    let centre = |a: &Anchor| match a {
        Anchor::Point(p) => Some(*p),
        Anchor::Body(n) => body(n).map(|(feet, h)| feet + DVec3::Y * h * 0.5),
    };
    for b in &mut fx.trainer.balls {
        let t = b.start.elapsed().as_secs_f32();
        let age = (t / b.secs.max(0.01)).clamp(0.0, 1.0);
        match b.phase {
            Phase::Fly { .. } => {
                let to = centre(&b.to).unwrap_or(b.at);
                let s = f64::from(age);
                let at = arc_at(b.from, to, s);
                let behind = arc_at(b.from, to, (s - 0.12).max(0.0));
                b.at = at;
                out.push(ball_effect(at, behind, 0, age, b.secs, [0.0, 0.0, 0.0, t * 14.0], 1.0, b.seed));
            }
            Phase::Landed => {
                let at = centre(&b.to).unwrap_or(b.at);
                b.at = at;
                out.push(ball_effect(at, at, 0, age, b.secs, [0.0; 4], 0.0, b.seed));
            }
            Phase::Thrown => {
                // Seen once the snowball is; `from` keeps where it was a frame ago, for the streak.
                let Some(at) = centre(&b.to).or(Some(b.at)).filter(|p| !p.is_nan()) else { continue };
                let before = if b.from.is_nan() { at } else { b.from };
                let behind = at - (at - before).normalize_or_zero() * 0.6;
                b.from = at;
                b.at = at;
                out.push(ball_effect(at, behind, 0, age, b.secs, [0.0, 0.0, 0.0, t * 14.0], 1.0, b.seed));
            }
            Phase::Miss => {
                // One low hop where it landed, fading.
                let hop = (std::f32::consts::PI * age).sin() * 0.35 * (1.0 - age);
                let at = b.at + DVec3::Y * f64::from(hop);
                out.push(ball_effect(at, at, 5, age, b.secs, [0.0, 0.0, 0.0, t * 6.0], 1.0 - age, b.seed));
            }
            Phase::Hold => {
                let ground = b.from;
                let target = centre(&b.to).unwrap_or(ground + DVec3::Y * 0.6);
                if t < OPEN_SECS {
                    let (lid, rise) = opening(t);
                    // Up from where it hit to above the target, then down to where it hangs.
                    let top = ground + DVec3::Y * (HOVER + 0.75);
                    let settle = ((t - 0.75) / 0.25).clamp(0.0, 1.0);
                    let bounce = (std::f32::consts::PI * settle).sin() * 0.12 * (1.0 - settle);
                    let hang = ground + DVec3::Y * (HOVER + f64::from(bounce));
                    let start = if b.at.distance(target) < 6.0 { b.at } else { target };
                    let at = if t < 0.75 { start.lerp(top, f64::from(rise)) } else { top.lerp(hang, f64::from(settle * settle * (3.0 - 2.0 * settle))) };
                    if t >= 0.2 {
                        b.at = top;
                    }
                    // The pull: strongest while the lid is fully open.
                    let pull = ((t - 0.12) / 0.1).clamp(0.0, 1.0) * (1.0 - ((t - 0.55) / 0.12).clamp(0.0, 1.0));
                    out.push(ball_effect(at, target, 1, t / OPEN_SECS, OPEN_SECS, [lid, 0.0, lid, 0.0], pull, b.seed));
                } else {
                    let w = t - OPEN_SECS;
                    let (tilt, glow) = wobble(w);
                    let bob = (w * 2.1).sin() * 0.04;
                    let at = ground + DVec3::Y * (HOVER + f64::from(bob));
                    b.at = at;
                    out.push(ball_effect(at, at, 2, age, b.secs, [0.0, tilt, glow, 0.0], 1.0, b.seed));
                }
            }
            Phase::Caught => {
                // A click, stars, then it drops and shrinks away.
                let sink = ((age - 0.55) / 0.45).clamp(0.0, 1.0);
                let at = b.at - DVec3::Y * f64::from(sink * sink * 0.5);
                let glow = (1.0 - age * 4.0).max(0.0);
                out.push(ball_effect(at, at, 3, age, b.secs, [0.0, 0.0, glow, 1.0 - sink], 1.0, b.seed));
            }
            Phase::Broke => out.push(ball_effect(b.at, b.at, 4, age, b.secs, [1.0, 0.0, 1.0, 0.0], 1.0, b.seed)),
            Phase::Treat => {
                let to = centre(&b.to).unwrap_or(b.at);
                let s = f64::from(age);
                let at = arc_at(b.from, to, s);
                b.at = at;
                let mut e = ball_effect(at, arc_at(b.from, to, (s - 0.12).max(0.0)), 0, age, b.secs, [0.0, 0.0, 0.0, t * 10.0], 1.0, b.seed);
                e.radius = 0.13;
                e.rgb = Vec3::new(1.0, 0.45, 0.7);
                out.push(e);
            }
        }
    }
    for r in &fx.trainer.ropes {
        let (Some((ff, fh)), Some((tf, th))) = (body(&r.from), body(&r.to)) else { continue };
        // From the trainer's hand to a loop round the target's middle.
        let a = ff + DVec3::Y * fh * 0.62;
        let b = tf + DVec3::Y * th * 0.5;
        let d = a.distance(b);
        let age = r.start.elapsed().as_secs_f32();
        // It flies out in the first quarter second, hangs slack when close and pulls taut far out.
        let reach = (age / 0.25).clamp(0.0, 1.0);
        let sag = ((9.0 - d) * 0.09).clamp(0.02, 0.7) as f32;
        let strain = ((d - 11.0) / 3.0).clamp(0.0, 1.0) as f32;
        let end = a.lerp(b, f64::from(reach));
        out.push(Effect {
            shape: Shape::Rope,
            a,
            b: end,
            radius: 0.055,
            rgb: Vec3::new(0.8, 0.64, 0.4),
            seed: 0,
            start: r.start,
            secs: r.secs,
            caster: String::new(),
            strength: 1.0,
            style: 0,
            flags: 0,
            look: [sag, if reach >= 1.0 { (th * 0.42).clamp(0.3, 1.6) as f32 } else { 0.0 }, strain, 0.0],
        });
    }
    for m in &fx.trainer.marks {
        // Over the head of a body, bobbing; a spot is where it is.
        let age = m.start.elapsed().as_secs_f32();
        let at = match &m.at {
            Anchor::Point(p) => Some(*p),
            Anchor::Body(n) => body(n).map(|(feet, h)| feet + DVec3::Y * (h + 0.55 + f64::from((age * 2.4).sin()) * 0.05)),
        };
        let Some(at) = at else { continue };
        // Drops in, and fades over its last half second.
        let pop = 1.0 + (1.0 - (age / 0.18).clamp(0.0, 1.0)) * 0.8;
        let fade = ((m.secs - age) / 0.5).clamp(0.0, 1.0);
        out.push(Effect {
            shape: Shape::Mark,
            a: at,
            b: at,
            radius: if m.kind == 3 { 0.9 } else { 0.3 * pop },
            rgb: if m.kind == 3 { Vec3::new(0.45, 0.75, 1.0) } else { Vec3::ONE },
            seed: 0,
            start: m.start,
            secs: m.secs,
            caster: String::new(),
            strength: fade,
            style: 0,
            flags: 0,
            look: [m.kind as f32, 0.0, 0.0, 0.0],
        });
    }
    let lines = fx.trainer.lines.iter().map(|l| (l.0, l.1, true, arc_height(l.0.distance(l.1))));
    for (a, b, locked, h) in fx.trainer.aim.into_iter().chain(lines) {
        out.push(Effect {
            shape: Shape::Arc,
            a,
            b,
            radius: 0.03,
            rgb: if locked { Vec3::new(1.0, 0.85, 0.3) } else { Vec3::ONE },
            seed: 0,
            start: Instant::now(),
            secs: 1.0,
            caster: String::new(),
            strength: 0.45,
            style: 0,
            flags: 0,
            look: [h as f32, f32::from(u8::from(locked)), 0.0, 0.0],
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flat ground at y 64 and nothing else.
    fn ground(a: [f64; 3], b: [f64; 3]) -> bool {
        a[1].min(b[1]) >= 64.0
    }

    fn thrown(pitch_deg: f64, bodies: &[([f64; 3], f64)], clear: &mut dyn FnMut([f64; 3], [f64; 3]) -> bool) -> Throw {
        let p = pitch_deg.to_radians();
        throw_path(throw_from([10.0, 65.62, 10.0]), throw_velocity([0.0, p.sin(), p.cos()]), bodies, clear)
    }

    #[test]
    fn a_level_throw_falls_to_the_ground_ahead_and_a_high_one_goes_further() {
        let flat = thrown(0.0, &[], &mut ground);
        assert!(flat.hit.is_none() && (flat.end[1] - 64.0).abs() < 0.2, "{:?}", flat.end);
        assert!((flat.end[0] - 10.0).abs() < 1e-6 && flat.end[2] > 18.0 && flat.end[2] < 30.0, "{:?}", flat.end);
        let high = thrown(35.0, &[], &mut ground);
        assert!(high.end[2] > flat.end[2] + 10.0 && high.secs > flat.secs * 2.0, "{:?} in {}", high.end, high.secs);
        // The drawn arc rises with the throw.
        assert!(high.arc > 5.0 && high.arc > flat.arc * 4.0 && thrown(-60.0, &[], &mut ground).arc < 0.3, "{} vs {}", high.arc, flat.arc);
        // Straight up comes back down close by.
        let up = thrown(89.0, &[], &mut ground);
        assert!(up.end[2] < 16.0 && up.secs > 2.0 && up.secs < 4.9, "{:?} in {}", up.end, up.secs);
    }

    #[test]
    fn a_throw_stops_at_the_first_body_or_wall_it_flies_into() {
        let body = ([10.0, 64.0, 16.0], 1.8);
        let hit = thrown(0.0, &[([10.0, 64.0, 22.0], 1.8), body], &mut ground);
        assert_eq!(hit.hit, Some(1));
        assert!((hit.end[2] - 16.0).abs() < 1.0 && hit.secs < 0.3, "{:?} in {}", hit.end, hit.secs);
        // Over a small one's head.
        assert_eq!(thrown(0.0, &[([10.0, 64.0, 16.0], 0.5)], &mut ground).hit, None);
        // A wall 4 blocks out stops the ball short of the body behind it.
        let mut wall = |a: [f64; 3], b: [f64; 3]| ground(a, b) && a[2].max(b[2]) < 14.0;
        let stopped = thrown(0.0, &[body], &mut wall);
        assert!(stopped.hit.is_none() && (stopped.end[2] - 14.0).abs() < 0.1, "{:?}", stopped.end);
    }

    #[test]
    fn the_hand_is_on_the_right() {
        // Facing south (+z, yaw 0), the right hand is to the west (-x).
        let h = hand([0.0, 64.0, 0.0], 0.0);
        assert!(h[0] < -0.3 && h[2].abs() < 1e-9 && h[1] > 65.0);
    }

    #[test]
    fn a_throw_starts_in_the_hand_peaks_in_the_middle_and_ends_on_the_mark() {
        let (a, b) = (DVec3::new(0.0, 65.0, 0.0), DVec3::new(10.0, 64.0, 0.0));
        assert!(arc_at(a, b, 0.0).distance(a) < 1e-9);
        assert!(arc_at(a, b, 1.0).distance(b) < 1e-9);
        let mid = arc_at(a, b, 0.5);
        assert!((mid.y - (64.5 + arc_height(a.distance(b)))).abs() < 1e-9);
        assert!(mid.y > arc_at(a, b, 0.2).y && mid.y > arc_at(a, b, 0.8).y);
    }

    #[test]
    fn a_long_throw_flies_longer_but_never_past_three_quarters_of_a_second() {
        assert!((flight_secs(1.0) - 0.25).abs() < 1e-6);
        assert!(flight_secs(12.0) > flight_secs(6.0));
        assert!((flight_secs(40.0) - 0.75).abs() < 1e-6);
    }

    #[test]
    fn the_lid_opens_then_shuts_before_the_wobbling_starts() {
        assert_eq!(opening(0.0).0, 0.0);
        assert_eq!(opening(0.4).0, 1.0);
        assert_eq!(opening(0.8).0, 0.0);
        assert!(opening(0.75).0 < 1e-6, "still open when it settles");
        assert!((opening(0.3).1 - 1.0).abs() < 1e-6);
    }

    #[test]
    fn the_ball_rocks_both_ways_then_rests() {
        let (mut lo, mut hi) = (0.0f32, 0.0f32);
        for i in 0..55 {
            let (tilt, _) = wobble(i as f32 * 0.01);
            lo = lo.min(tilt);
            hi = hi.max(tilt);
        }
        assert!(hi > 0.25 && lo < -0.25, "tilt {lo}..{hi}");
        assert_eq!(wobble(WOBBLE_ROCK + 0.1), (0.0, 0.0));
        // The next cycle rocks again.
        assert!(wobble(WOBBLE_SECS + 0.07).0.abs() > 0.05);
    }

    #[test]
    fn anchors_are_points_or_named_bodies() {
        assert_eq!(Anchor::parse("@Pippa"), Some(Anchor::Body("Pippa".into())));
        assert_eq!(Anchor::parse("@e:42"), Some(Anchor::Body("e:42".into())));
        assert_eq!(Anchor::parse("1,2,3"), Some(Anchor::Point(DVec3::new(1.0, 2.0, 3.0))));
        assert_eq!(Anchor::parse("@"), None);
        assert_eq!(Anchor::parse("nope"), None);
    }

    fn ball_phase(fx: &Fx, id: &str) -> Option<Phase> {
        fx.trainer.balls.iter().find(|b| b.id == id).map(|b| b.phase)
    }

    #[test]
    fn a_ball_flies_holds_and_ends_as_the_server_says() {
        let mut fx = Fx::default();
        assert!(heard(&mut fx, &["tball", "bob", "fly", "0,65,0", "@bob", "0.4", "1"]));
        assert_eq!(ball_phase(&fx, "bob"), Some(Phase::Fly { hit: true }));
        assert!(heard(&mut fx, &["tball", "bob", "hold", "@bob", "5,64,0", "6.0"]));
        assert_eq!(ball_phase(&fx, "bob"), Some(Phase::Hold));
        assert_eq!(fx.trainer.balls.len(), 1, "the flying ball became the holding one");
        assert!(heard(&mut fx, &["tball", "bob", "out", "0"]));
        assert_eq!(ball_phase(&fx, "bob"), Some(Phase::Broke));
        assert!(heard(&mut fx, &["tball", "amy", "hold", "-", "1,64,1", "6.0"]));
        assert!(heard(&mut fx, &["tball", "amy", "out", "1"]));
        assert_eq!(ball_phase(&fx, "amy"), Some(Phase::Caught));
        // An `out` for a ball this client never saw is taken and does nothing.
        assert!(heard(&mut fx, &["tball", "zed", "out", "1"]));
        assert_eq!(ball_phase(&fx, "zed"), None);
    }

    #[test]
    fn a_rope_needs_both_bodies_and_goes_when_told() {
        let mut fx = Fx::default();
        assert!(heard(&mut fx, &["rope", "r1", "@Pippa", "@bob", "10"]));
        let here = |n: &str| (n == "Pippa").then_some((DVec3::new(0.0, 64.0, 0.0), 1.8));
        assert!(effects(&mut fx, &here).is_empty(), "drawn to a body that isn't there");
        let both = |n: &str| Some((if n == "Pippa" { DVec3::new(0.0, 64.0, 0.0) } else { DVec3::new(6.0, 64.0, 0.0) }, 1.8));
        let drawn = effects(&mut fx, &both);
        assert_eq!(drawn.len(), 1);
        assert_eq!(drawn[0].shape, Shape::Rope);
        assert!(heard(&mut fx, &["rope", "r1", "-"]));
        assert!(effects(&mut fx, &both).is_empty());
    }

    #[test]
    fn a_far_rope_is_taut_and_strained() {
        let mut fx = Fx::default();
        heard(&mut fx, &["rope", "r1", "@a", "@b", "10"]);
        fx.trainer.ropes[0].start = Instant::now().checked_sub(std::time::Duration::from_secs(1)).unwrap();
        let at = |d: f64| move |n: &str| Some((if n == "a" { DVec3::ZERO } else { DVec3::new(d, 0.0, 0.0) }, 1.0));
        let near = effects(&mut fx, &at(3.0))[0].look;
        let far = effects(&mut fx, &at(13.5))[0].look;
        assert!(near[0] > far[0], "sag {} vs {}", near[0], far[0]);
        assert_eq!(near[2], 0.0);
        assert!(far[2] > 0.7);
    }
}
