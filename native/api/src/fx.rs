//! Move effects: glowing beams, streaks, shells and hit flashes, drawn in one full-screen pass
//! after the world (`RenderHooks::world_drawn`), plus camera shake and coloured light.
//!
//! The server says what happened (`fx`, `hit` and `guard` messages); each client draws it
//! locally. A beam ends where the server's line of sight stopped, so every client agrees.
//!
//! Techniques adapted from Orbital Laser (MIT, (c) 2026 Stink-O, commit 7853371): the world
//! position rebuilt from the depth buffer, and beams drawn as the distance from each pixel's
//! view ray to the beam's axis. No code is copied; see `licences/orbital-laser.txt` in the
//! content mod. The Z-Move cinematic's staging (sky pillar, shock rings, one flash, shake and
//! booms by distance) is inspired by its strikes too: docs/fx/ZMOVE_CINEMATIC.md.
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
    /// A Z-Move's build-up on the caster (docs/fx/ZMOVE_CINEMATIC.md): rising aura, energy closing
    /// in, the ground cracking and glowing under its feet.
    ZGather = 12,
    /// A Z-Move's release and aftermath: a sky pillar slamming onto the impact point, the move's
    /// line (flags: 1 beam, 2 burst, 3 dash, 4 cone, 5 ground), shock rings, scorch, embers. Flags
    /// 6 is a healing Z-Move: a column of light rising, soft rings out to its reach, motes.
    ZRelease = 13,
    /// A Rift Capsule opening in front of the camera (`capsule_begin`): flags is its tier.
    Capsule = 14,
    /// The trainer kit (`fx_trainer.rs`): a Poké Ball (flags: 0 flying, 1 opening on its target,
    /// 2 hanging and wobbling, 3 caught, 4 burst open, 5 a miss fading), the Tether Rope, and
    /// the faint line of a throw being aimed.
    Ball = 15,
    Rope = 16,
    Arc = 17,
    /// A trainer's order over its target: a bullseye, a Z, or a ring on the ground.
    Mark = 18,
    /// A legendary aura (cosmetics, `cosmetics_draw::legend`): a rune ring on the ground, a sheath
    /// of light, pillars and a flare every few seconds, by theme (`style`). Never takes a move's slot.
    /// (15-18 are left to the trainer kit's shapes.)
    Legend = 19,
    /// A woken legendary shrine (`shrine` messages, server/shrine_fx.rs; docs/fx/SHRINE_FX.md): flags
    /// 0 its meteor falling, 1 awake, 2 being captured, 3 contested, 4 the crown.
    Shrine = 20,
}

/// The trainer kit's shapes are part of play, not decoration: Low FX keeps them, they are drawn
/// from further off and they get a slot before any move effect.
fn kit_shape(s: Shape) -> bool {
    matches!(s, Shape::Ball | Shape::Rope | Shape::Arc | Shape::Mark)
}

/// A Potion's green glow, as an `Aura`'s flags (the shader's status number).
const AURA_HEAL: u32 = 7;

#[path = "fx_trainer.rs"]
pub mod trainer;

/// Each type's shrine arrival on the CPU side: its focus, rumble, sounds and jolts.
#[path = "fx_shrine.rs"]
pub mod arrival;

/// At most this many legendary auras get the effects pass and a light, the nearest first.
const LEGEND_MAX: usize = 4;
const LEGEND_LIGHTS: usize = 3;
/// They fade out between these distances (blocks); the sprites stay to 32.
const LEGEND_FADE: (f64, f64) = (20.0, 30.0);

/// A Z-Move's release and aftermath last this long (s).
const Z_RELEASE_SECS: f32 = 2.6;
/// The build-up shows for at least this long, even when the move fires at once (s).
const Z_GATHER_MIN: f32 = 0.35;
/// A `zmove` waits this long past its wind-up for the move's `fx` before it's forgotten (s).
const Z_ARM_GRACE: f32 = 2.0;
/// A healing Z-Move's release (`Effect::flags`, beside the forms 1-5), and its build-up's flag.
const Z_HEAL_FORM: u32 = 6;
const Z_HEAL_GATHER: u32 = 1;
/// A healing Z-Move shakes the camera this much of a hitting one's.
const Z_HEAL_SHAKE: f32 = 0.2;
/// Z-Move effects are drawn (and shake the camera) from this far away (blocks).
const Z_CULL: f64 = 96.0;
/// Sound travels this fast (blocks a second): a far Z-Move's boom arrives after its flash.
const SOUND_SPEED: f64 = 343.0;

/// A shrine and its meteor are drawn from this far (blocks); the vanilla beacon beam carries further.
const SHRINE_CULL: f64 = 640.0;
/// The column's height over the shrine (the shader's `H`), for sorting by distance.
const SHRINE_TOP: f64 = 96.0;
/// The crown lasts this long (s).
const SHRINE_CROWN_SECS: f32 = 3.2;
/// A shrine the server stopped mentioning is dropped after this long (s); it repeats every half second.
const SHRINE_STALE: f32 = 4.0;
/// Where a meteor starts, from its shrine: this far out (in a direction from its seed) and up.
const METEOR_FROM: (f64, f64) = (48.0, 150.0);

/// One woken shrine as the server last described it (`shrine` messages).
#[derive(Clone, Debug)]
struct ShrineFx {
    id: String,
    at: DVec3,
    kind: String,
    /// 0 falling, 1 awake, 2 being captured, 3 contested.
    phase: u32,
    /// The capture count (0-1) the server last sent, and the one drawn (eased toward it).
    progress: f32,
    shown: f32,
    woke: Instant,
    phase_at: Instant,
    seen: Instant,
    /// How long its meteor falls (s), and whether this client has played the landing.
    fall: f32,
    landed: bool,
    /// Whether its build-up's sounds and jolts are scheduled (`arrival::cues`).
    cued: bool,
    seed: u32,
    /// The capture ring's radius (blocks): where a Pokemon must stand.
    ring: f32,
}

impl ShrineFx {
    fn woke_secs(&self) -> f32 {
        self.woke.elapsed().as_secs_f32()
    }

    /// Where its arrival is centred now (fire: its meteor; the dragon, the eclipse, the ball...).
    fn focus(&self) -> DVec3 {
        arrival::focus(&self.kind, self.at, self.seed, self.woke_secs().min(self.fall), self.fall)
    }
}

/// Where a shrine's meteor is `k` (0-1) of the way down: it drifts in slowly from high up and off to
/// one side, then speeds up into the slam (the shader is handed this point).
pub fn meteor_at(at: DVec3, seed: u32, k: f32) -> DVec3 {
    let a = f64::from(seed % 360).to_radians();
    let from = at + DVec3::new(a.cos() * METEOR_FROM.0, METEOR_FROM.1, a.sin() * METEOR_FROM.0);
    let end = at + DVec3::Y;
    from.lerp(end, f64::from(k.clamp(0.0, 1.0)).powf(2.2))
}

/// How much of a far blast reaches a listener `d` blocks away (Orbital Laser's falloff, a third at
/// 300 blocks): shake strength reads it.
fn far_falloff(d: f64) -> f32 {
    (1.4 / (1.0 + d / 100.0)).min(1.0) as f32
}

/// The landing as heard `d` blocks away: (sound, volume, pitch, seconds after the sound arrives).
/// Distance tiers, each a different vanilla sound (near: the blast and the type's crack; mid: the
/// thunder rolls; far: only the low thunder), and no two big hits within 0.15 s (they would flam).
fn landing_booms(d: f64, kind: &str) -> Vec<(&'static str, f32, f32, f32)> {
    if d < 64.0 {
        vec![
            ("minecraft:entity.generic.explode", 2.5, 0.55, 0.0),
            (hit_sound(kind), 1.6, 0.5, 0.02),
            ("minecraft:entity.lightning_bolt.thunder", 1.6, 0.7, 0.2),
            ("minecraft:block.beacon.activate", 1.2, 0.75, 1.1),
        ]
    } else if d < 220.0 {
        vec![("minecraft:entity.lightning_bolt.thunder", 1.0, 0.6, 0.0), ("minecraft:entity.generic.explode", 0.8, 0.45, 0.2)]
    } else {
        vec![("minecraft:entity.lightning_bolt.thunder", 0.7, 0.5, 0.0)]
    }
}

/// A Z-Move announced by `zmove`, waiting for its `fx`.
#[derive(Clone, Debug)]
struct ZArm {
    id: String,
    /// When it was announced, or when it left if it lands later (`launched`).
    at: Instant,
    /// How long until its `fx` is due from `at`: the wind-up, then the delay once it has left.
    wait: f32,
    /// It has left and lands later (Future Sight, Extrasensory, meteors): the server announced
    /// the delay with `charge <who>~<tag> ...`, and the `fx` comes under that tagged name.
    launched: bool,
    /// Where its warning said it lands, once it has left.
    land: Option<DVec3>,
    /// A healing Z-Move (`zmove ... heal`): the gentle cinematic.
    heal: bool,
}

impl ZArm {
    fn live(&self) -> bool {
        self.at.elapsed().as_secs_f32() < self.wait + Z_ARM_GRACE
    }

    /// Whether an `fx` of `id` from `at` (tagged: under `<name>~<tag>`) is this Z-Move landing.
    /// A delayed one only lands tagged, after its warning, where the warning said: an earlier
    /// slots 1-4 Future Sight still in flight lands tagged too, but elsewhere or before it.
    fn lands(&self, id: &str, tagged: bool, at: DVec3) -> bool {
        self.id == id
            && self.live()
            && if tagged { self.launched && self.land.is_none_or(|p| p.distance(at) < 1.5) } else { !self.launched }
    }
}

/// The player behind a caster name: delayed strikes go out as `<name>~<tag>` (server/play.rs
/// `fire`, server/sig.rs meteors). Returns (name, whether it was tagged).
fn caster_name(caster: &str) -> (&str, bool) {
    match caster.split_once('~') {
        Some((name, _)) => (name, true),
        None => (caster, false),
    }
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
    /// Casters whose Z-Move is winding up, by player name. Their next `fx` of that move gets the
    /// release cinematic.
    zarmed: HashMap<String, ZArm>,
    /// Camera shake from Z-Moves near the camera this frame (0-1), worked out in `frame`.
    zshake: f32,
    /// World sounds that wait (sound, where, volume, pitch, when): Z-Move booms, timed so they
    /// arrive after the flash by the distance (`SOUND_SPEED`).
    later: Vec<(&'static str, DVec3, f32, f32, Instant)>,
    /// A Rift Capsule opening, if one is playing.
    capsule: Option<CapsuleFx>,
    /// Thrown balls, captures, ropes and the aim line (`fx_trainer.rs`).
    trainer: trainer::State,
    /// Woken legendary shrines (`shrine` messages).
    shrines: Vec<ShrineFx>,
    /// Shrines crowned since the last frame (their jolt needs the camera's distance).
    crowned: Vec<DVec3>,
    /// Camera jolts that stack (start, seconds, strength): each fades as (1 - u)^2 and they add as
    /// the root of the sum of squares (Orbital Laser's trauma model, written again here).
    jolts: Vec<(Instant, f32, f32)>,
    /// A falling meteor's rumble near the camera this frame (0-1), worked out in `frame`.
    rumble: f32,
    /// World sounds due at a set time, already delayed by their distance (no cap): shrine booms.
    timed: Vec<(&'static str, DVec3, f32, f32, Instant)>,
    /// `/capifx flash off`: no whole-picture flashes (a Z-Move's release, a shrine's crown).
    no_flash: bool,
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
            "hit" => Effect { a: at(x, -0.7, 7.0), b: at(x, -0.7, 7.0), radius: hit_size(14.0), secs: hit_secs(0), ..base },
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

/// A hit flash's radius for the damage dealt. (0.45 + dmg / 40 up to 1.2 was a 10-pixel blink from
/// 15 blocks: docs/fx/MOVE_FX_VISIBILITY.md. `native/tools/fx_contrast.py` reads this formula.)
fn hit_size(dmg: f32) -> f32 {
    (0.6 + dmg / 35.0).min(1.5)
}

/// How long a hit flash lasts: longer for a bite, punch or cut, which draw their own shape (was 0.4 / 0.3).
fn hit_secs(flags: u32) -> f32 {
    if flags & 7 != 0 { 0.5 } else { 0.42 }
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

/// A Z-Move left (its `fx` after a `zmove`): the release and aftermath, and its booms. `e` is the
/// move's own effect, which still draws (it's the real hitbox); this goes on top. A healing one
/// (`heal`) rises from the caster instead of slamming down, with chimes for booms.
fn z_release(fx: &mut Fx, e: &Effect, kind: &str, heal: bool) {
    // Where it starts and where it lands, by the move's shape.
    let (form, a, b) = match e.shape {
        _ if heal => (Z_HEAL_FORM, e.a, e.a),
        Shape::Beam => (1, e.a, e.b),
        Shape::Burst => (2, e.a, e.a),
        Shape::Dash => (3, e.a, e.b),
        Shape::Cone => (4, e.a, e.b),
        // A field's zone: a disc (a == b) or a strip; it lands where it starts.
        Shape::Zone => (5, e.a, e.a),
        _ => (5, e.b, e.b),
    };
    let now = Instant::now();
    fx.effects.push(Effect {
        shape: Shape::ZRelease,
        a,
        b,
        radius: e.radius.clamp(0.4, 24.0),
        rgb: e.rgb,
        seed: e.seed,
        start: now,
        secs: Z_RELEASE_SECS,
        caster: e.caster.clone(),
        strength: 1.0,
        style: e.style,
        flags: form,
        look: [0.0; 4],
    });
    if fx.effects.len() > 64 {
        fx.effects.remove(0);
    }
    // The slam, the type's own crack under it, a high crackle, and the hum dying away. A heal
    // chimes instead.
    let after = |s: f32| now + std::time::Duration::from_secs_f32(s);
    let booms = if heal {
        [
            ("minecraft:block.beacon.power_select", 1.0, 1.25, 0.0),
            ("minecraft:block.amethyst_block.chime", 1.2, 0.8, 0.0),
            ("minecraft:block.amethyst_block.chime", 1.0, 1.2, 0.25),
            ("minecraft:block.beacon.deactivate", 0.5, 1.3, 1.4),
        ]
    } else {
        [
            ("minecraft:entity.generic.explode", 1.2, 0.7, 0.0),
            (hit_sound(kind), 1.0, 0.6, 0.0),
            ("minecraft:entity.lightning_bolt.thunder", 0.45, 1.6, 0.05),
            ("minecraft:block.beacon.deactivate", 0.8, 0.8, 1.2),
        ]
    };
    for (sound, vol, pitch, at) in booms {
        if fx.later.len() < 16 {
            fx.later.push((sound, b, vol, pitch, after(at)));
        }
    }
}

/// How hard a Z-Move shakes a camera `dist` blocks away: the build-up ramps (`gather` true, age
/// 0-1 of the wind-up), then the release jolts hard and settles (age 0-1 of the aftermath).
fn z_shake(gather: bool, age: f32, dist: f64) -> f32 {
    let near = (1.0 - dist as f32 / 40.0).clamp(0.0, 1.0);
    if near <= 0.0 {
        return 0.0;
    }
    let k = if gather {
        0.15 + 0.4 * age.clamp(0.0, 1.0).powi(2)
    } else {
        let t = age.clamp(0.0, 1.0) * Z_RELEASE_SECS;
        // The jolt lands with the pillar (0.1 s), then a rumble that dies over a second or so.
        let jolt = if t < 0.1 { t / 0.1 * 0.5 } else { 0.95 * (-(t - 0.1) * 2.2).exp() };
        jolt + 0.12 * (1.0 - t / Z_RELEASE_SECS)
    };
    (k * near).min(1.0)
}

// ---------------------------------------------------------------- Rift Capsule opening

/// A capsule opening (docs/fx/CAPSULE_OPENING.md): it appears in front of the camera, shakes and
/// charges, bursts with a flash and a beam in its rarity's colour, and the prize circles where it
/// was. Lumi's menu or `/capsule open` (`riftmenu.rs`); the server rolled the prize already.
pub const CAPSULE_BURST: f32 = 2.6;
/// The reveal card is gone by then (s).
pub const CAPSULE_END: f32 = 7.0;
/// A key skips from this far in (s).
pub const CAPSULE_SKIP_AFTER: f32 = 1.0;
/// Past the burst, a skip only closes it after this long (so one press doesn't skip both).
const CAPSULE_CLOSE_AFTER: f32 = 0.4;
/// How far in front of the eye it floats (blocks), and how far below the eye line.
const CAPSULE_AHEAD: f64 = 2.3;
const CAPSULE_DROP: f64 = 0.2;
/// Its radius (blocks).
const CAPSULE_SIZE: f32 = 0.24;
/// The shader's clock span: age = t / this.
const CAPSULE_SECS: f32 = 8.0;

#[derive(Clone, Debug)]
struct CapsuleFx {
    /// 0 common, 1 rare, 2 legendary.
    tier: u32,
    rgb: Vec3,
    start: Instant,
    /// Seconds a skip jumped ahead.
    ahead: f32,
    /// Where it floats, from the camera this frame (sounds and light; the effects pass places it
    /// again from the camera it draws with, `capsule_drawn`).
    at: Option<DVec3>,
    /// The camera's yaw then.
    yaw: f32,
    /// Sounds from `CAPSULE_SOUNDS` already played (or skipped).
    played: usize,
    seed: u32,
    /// A trail prize: its cosmetic id, so its own particles circle the burst (Onii, 2026-11-20).
    trail: Option<String>,
}

impl CapsuleFx {
    fn t(&self) -> f32 {
        self.start.elapsed().as_secs_f32() + self.ahead
    }
}

/// (when, sound, volume, pitch, lowest tier that hears it), in time order.
const CAPSULE_SOUNDS: [(f32, &str, f32, f32, u32); 17] = [
    (0.0, "minecraft:block.amethyst_block.chime", 1.0, 1.0, 0),
    (0.05, "minecraft:block.beacon.activate", 0.5, 1.8, 0),
    (0.55, "minecraft:block.respawn_anchor.charge", 0.6, 0.9, 0),
    (0.75, "minecraft:block.chest.locked", 0.6, 0.8, 0),
    (1.2, "minecraft:block.chest.locked", 0.65, 0.9, 0),
    (1.55, "minecraft:block.respawn_anchor.charge", 0.75, 1.2, 0),
    (1.6, "minecraft:block.chest.locked", 0.7, 1.0, 0),
    (1.9, "minecraft:block.chest.locked", 0.75, 1.15, 0),
    (2.15, "minecraft:block.respawn_anchor.charge", 0.9, 1.6, 1),
    (2.2, "minecraft:block.chest.locked", 0.8, 1.3, 0),
    (2.42, "minecraft:block.chest.locked", 0.85, 1.5, 0),
    (CAPSULE_BURST, "minecraft:entity.firework_rocket.large_blast", 1.0, 0.9, 0),
    (CAPSULE_BURST, "minecraft:block.glass.break", 0.8, 1.2, 0),
    (CAPSULE_BURST + 0.02, "minecraft:entity.generic.explode", 0.45, 1.5, 1),
    (CAPSULE_BURST + 0.05, "minecraft:item.totem.use", 0.7, 1.0, 2),
    (CAPSULE_BURST + 0.15, "minecraft:entity.player.levelup", 0.9, 1.0, 1),
    (CAPSULE_BURST + 0.2, "minecraft:block.beacon.power_select", 0.7, 1.4, 0),
];

/// Starts a capsule opening of `tier` (0-2) in its rarity's colour (0xRRGGBB).
pub fn capsule_begin(tier: u32, rgb: u32) {
    capsule_begin_with(tier, rgb, None);
}

/// `capsule_begin` for a prize that's a trail: after the burst its particles circle where the
/// capsule was (`capsule_trail`, drawn by `cosmetics::draw::capsule_trail`).
pub fn capsule_begin_with(tier: u32, rgb: u32, trail: Option<String>) {
    let c = |s: u32| ((rgb >> s) & 0xFF) as f32 / 255.0;
    let mut fx = FX.lock().unwrap();
    let seed = fx.frames.wrapping_mul(2_654_435_761) % 997;
    fx.capsule = Some(CapsuleFx { tier: tier.min(2), rgb: Vec3::new(c(16), c(8), c(0)), start: Instant::now(), ahead: 0.0, at: None, yaw: 0.0, played: 0, seed, trail });
}

/// A trail prize's preview: its id, where the capsule burst and seconds since the burst. None
/// before the burst, after the card, or for a prize that isn't a trail.
pub fn capsule_trail() -> Option<(String, DVec3, f32)> {
    let fx = FX.lock().unwrap();
    let c = fx.capsule.as_ref()?;
    let t = c.t();
    (t >= CAPSULE_BURST && t < CAPSULE_END).then_some(())?;
    Some((c.trail.clone()?, c.at?, t - CAPSULE_BURST))
}

/// Seconds into the opening, if one is playing.
pub fn capsule_time() -> Option<f32> {
    FX.lock().unwrap().capsule.as_ref().map(CapsuleFx::t)
}

/// Where a skip at `t` goes: Some(new t), or None to close it. Nothing in the first second, and
/// just after the burst a press doesn't close it yet.
pub fn capsule_skip_to(t: f32) -> Option<f32> {
    if t < CAPSULE_SKIP_AFTER {
        Some(t)
    } else if t < CAPSULE_BURST {
        Some(CAPSULE_BURST)
    } else if t < CAPSULE_BURST + CAPSULE_CLOSE_AFTER {
        Some(t)
    } else {
        None
    }
}

/// A skip key: on to the burst, or closed once the prize is showing.
pub fn capsule_skip() {
    let mut fx = FX.lock().unwrap();
    let Some(c) = fx.capsule.as_mut() else { return };
    match capsule_skip_to(c.t()) {
        Some(t) => c.ahead += t - c.t(),
        None => fx.capsule = None,
    }
}

pub fn capsule_stop() {
    FX.lock().unwrap().capsule = None;
}

/// Whether the HUD draws the capsule instead of the effects pass: on Low FX, or when the pass
/// switched itself off (MSAA, a rejected pipeline or depth binding).
fn capsule_flat_for(low: bool, pass_off: bool) -> bool {
    low || pass_off
}

pub fn capsule_flat() -> bool {
    capsule_flat_for(low(), RENDER.off.load(Ordering::Relaxed))
}

/// How hard the opening shakes the camera at `t`: a growing rattle, a jolt at the burst.
fn capsule_shake(t: f32, tier: u32) -> f32 {
    if t < 0.45 {
        0.0
    } else if t < CAPSULE_BURST {
        let ch = (t - 0.45) / (CAPSULE_BURST - 0.45);
        0.08 + 0.22 * ch * ch
    } else {
        (0.45 + 0.12 * tier as f32) * (-(t - CAPSULE_BURST) * 3.0).exp()
    }
}

/// The sounds due at `t` from `played` on: the ones to play, and the new `played`. Sounds a skip
/// jumped over (more than 0.3 s late, before the burst) are dropped, not played in a heap.
fn capsule_sounds(played: usize, t: f32, tier: u32) -> (Vec<(&'static str, f32, f32)>, usize) {
    let mut out = Vec::new();
    let mut n = played;
    while let Some(&(at, sound, vol, pitch, min)) = CAPSULE_SOUNDS.get(n) {
        if at > t {
            break;
        }
        n += 1;
        if tier < min || (at < CAPSULE_BURST && t - at > 0.3) {
            continue;
        }
        out.push((sound, vol, pitch));
    }
    (out, n)
}

/// The capsule's place: ahead of the camera where it looks (yaw, and pitch kept within 25 degrees).
fn capsule_place(eye: DVec3, yaw: f32, pitch: f32) -> DVec3 {
    let (y, p) = (f64::from(yaw).to_radians(), f64::from(pitch.clamp(-25.0, 25.0)).to_radians());
    let dir = DVec3::new(-y.sin() * p.cos(), -p.sin(), y.cos() * p.cos());
    eye + dir * CAPSULE_AHEAD - DVec3::Y * CAPSULE_DROP
}

/// The same, from the camera's view direction `fwd` (`yaw`: the facing to use when it looks
/// straight up or down, where `fwd` has none).
fn capsule_place_along(camera: DVec3, fwd: DVec3, yaw: f32) -> DVec3 {
    let f = fwd.normalize_or_zero();
    let flat = (f.x * f.x + f.z * f.z).sqrt();
    let yaw = if flat > 1e-3 { (-f.x).atan2(f.z).to_degrees() as f32 } else { yaw };
    let pitch = (-f.y).clamp(-1.0, 1.0).asin().to_degrees() as f32;
    capsule_place(camera, yaw, pitch)
}

/// The view direction a camera-relative view-projection matrix looks along (the screen's centre).
fn view_forward(view_proj: Mat4, eye: DVec3) -> Option<DVec3> {
    let q = view_proj.as_dmat4().inverse() * glam::DVec4::new(0.0, 0.0, 0.5, 1.0);
    let d = q.truncate() / q.w - eye;
    (q.w.abs() > 1e-12 && d.is_finite() && d.length_squared() > 1e-12).then(|| d.normalize())
}

/// The opening as the shader draws it this frame: placed from the camera the world is drawn with
/// (over the shoulder, first person, the photo camera, whichever), so it sits CAPSULE_AHEAD in
/// front of the real lens on the view axis. Without a view direction, where `frame` last put it.
fn capsule_drawn(c: &CapsuleFx, camera: DVec3, fwd: Option<DVec3>) -> Option<Effect> {
    let mut e = capsule_effect(c)?;
    if let Some(f) = fwd {
        let at = capsule_place_along(camera, f, c.yaw);
        (e.a, e.b) = (at, at);
    }
    Some(e)
}

/// The opening as the shader draws it (`capfx`), at its last place, or None before it has one.
fn capsule_effect(c: &CapsuleFx) -> Option<Effect> {
    let at = c.at?;
    Some(Effect {
        shape: Shape::Capsule,
        a: at,
        b: at,
        radius: CAPSULE_SIZE,
        rgb: c.rgb,
        seed: c.seed,
        // So `age` is the opening's own clock (a skip moves it on).
        start: Instant::now().checked_sub(std::time::Duration::from_secs_f32(c.t().clamp(0.0, CAPSULE_SECS))).unwrap_or_else(Instant::now),
        secs: CAPSULE_SECS,
        caster: String::new(),
        strength: 1.0,
        style: 0,
        flags: c.tier,
        look: [CAPSULE_BURST, c.tier as f32, 0.0, 0.0],
    })
}

/// A server message for the effects, if it is one. `me` is this client's player name.
pub fn heard(f: &[&str], me: Option<&str>) -> bool {
    let mut fx = FX.lock().unwrap();
    if trainer::heard(&mut fx, f) {
        return true;
    }
    match f {
        // shrine <id> <x,y,z> <type> <fall|awake|capture|contest|crown|gone> <progress> <age s> <fall s> <seed> <ring>
        ["shrine", id, at, kind, phase, prog, age, fall, seed, ring, ..] => {
            let Some(at) = parse_v3(at) else { return true };
            let num = |s: &str, d: f32| s.parse::<f32>().ok().filter(|v| v.is_finite()).unwrap_or(d);
            let ring = num(ring, 5.0).clamp(1.0, 16.0);
            shrine_heard(&mut fx, id, at, kind, phase, num(prog, 0.0).clamp(0.0, 1.0), num(age, 0.0).clamp(0.0, 3600.0), num(fall, 6.0).clamp(0.5, 20.0), seed.parse().unwrap_or(0), ring);
            true
        }
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
                radius: radius.parse::<f32>().unwrap_or(0.4).clamp(0.05, 24.0),
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
            // A Z-Move announced by `zmove` (its wind-up, if this is its `fx`): the release
            // cinematic goes on top of the move's own look. Rain Dance and Tempest (fields) send
            // only their zone, so a zone releases it too. A delayed strike (Future Sight) lands
            // under `<name>~<tag>`; once one has left, a plain `fx` of the same id from that
            // player is a slots 1-4 cast, not the Z-Move.
            let (zname, tagged) = caster_name(caster);
            let zwind = fx.zarmed.get(zname).filter(|z| z.lands(id, tagged, a)).map(|z| (z.wait, z.heal));
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
            if *id != "dodge" {
                if let Some((w, heal)) = zwind {
                    // A Z-Move that fired at once keeps its build-up for its short minimum (it
                    // came in the same tick as this `fx` and would never have been drawn).
                    if w >= Z_GATHER_MIN {
                        fx.effects.retain(|x| !(x.caster == zname && x.shape == Shape::ZGather));
                    }
                    fx.zarmed.remove(zname);
                    z_release(&mut fx, &e, kind, heal);
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
                radius: hit_size(dmg),
                rgb: type_rgb(kind),
                seed: dmg as u32,
                start: Instant::now(),
                secs: hit_secs(flags),
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
            fx.effects.retain(|x| !(x.caster == *who && matches!(x.shape, Shape::Charge | Shape::Warn | Shape::ZGather)));
            // A Z-Move that has already left (and lands later) can't be stopped any more.
            if fx.zarmed.get(*who).is_some_and(|z| !z.launched) {
                fx.zarmed.remove(*who);
            }
            false
        }
        // zmove <who> <move> <type> <secs> [heal]: a Z-Move is being cast (sent before its `charge`
        // and `fx`); <secs> is its wind-up, 0 when it fires at once. `heal`: a healing one.
        ["zmove", who, id, kind, secs, rest @ ..] => {
            let secs = secs.parse::<f32>().ok().filter(|s| s.is_finite()).unwrap_or(0.0).clamp(0.0, 5.0);
            let heal = rest.first() == Some(&"heal");
            fx.zarmed.retain(|_, z| z.live());
            fx.zarmed.insert(who.to_string(), ZArm { id: id.to_string(), at: Instant::now(), wait: secs, launched: false, land: None, heal });
            fx.effects.retain(|x| !(x.caster == *who && x.shape == Shape::ZGather));
            fx.effects.push(Effect {
                shape: Shape::ZGather,
                a: DVec3::ZERO,
                b: DVec3::ZERO,
                radius: 1.0,
                rgb: type_rgb(kind),
                seed: 31 + secs.to_bits() % 613,
                start: Instant::now(),
                secs: secs.max(Z_GATHER_MIN),
                caster: who.to_string(),
                strength: 1.0,
                style: type_index(kind),
                flags: if heal { Z_HEAL_GATHER } else { 0 },
                look: [0.0; 4],
            });
            if fx.effects.len() > 64 {
                fx.effects.remove(0);
            }
            let charge = if heal { ("minecraft:block.amethyst_block.resonate", 1.0, 0.7) } else { ("minecraft:block.respawn_anchor.charge", 0.8, 0.6) };
            for (sound, vol, pitch) in [("minecraft:block.beacon.activate", 1.0, 1.3), charge] {
                if fx.sounds.len() < 8 {
                    fx.sounds.push((sound, Err(who.to_string()), vol, pitch));
                }
            }
            true
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
                    let radius = r.parse::<f32>().unwrap_or(1.0).clamp(0.2, 24.0);
                    fx.effects.push(Effect { shape: Shape::Warn, a, b, radius, flags: kindw, ..base.clone() });
                }
            }
            // A delayed strike's warning (`<name>~<tag>`): an armed Z-Move has left and lands
            // `secs` from now, so its arm waits that long for the `fx`. Its warning shows as usual.
            let (zname, tagged) = caster_name(who);
            if tagged {
                if let Some(arm) = fx.zarmed.get_mut(zname).filter(|z| z.id == *id && z.live() && !z.launched) {
                    let land = rest.first().and_then(|p| parse_v3(p));
                    *arm = ZArm { at: Instant::now(), wait: secs, launched: true, land, ..arm.clone() };
                }
            }
            // A Z-Move's wind-up is drawn by its build-up (ZGather), not the plain gathering ring.
            let z = !tagged && fx.zarmed.get(*who).is_some_and(|z| z.id == *id && !z.launched);
            if !z {
                fx.effects.push(base);
            }
            if !z && fx.sounds.len() < 8 {
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

/// A `shrine` line: the shrine appears, changes state, crowns or goes.
#[allow(clippy::too_many_arguments)]
fn shrine_heard(fx: &mut Fx, id: &str, at: DVec3, kind: &str, phase: &str, prog: f32, age: f32, fall: f32, seed: u32, ring: f32) {
    let now = Instant::now();
    let code = match phase {
        "fall" => 0,
        "awake" => 1,
        "capture" => 2,
        "contest" => 3,
        "crown" => {
            fx.shrines.retain(|s| s.id != id);
            shrine_crown(fx, at, kind, seed);
            return;
        }
        _ => {
            fx.shrines.retain(|s| s.id != id);
            return;
        }
    };
    if let Some(s) = fx.shrines.iter_mut().find(|s| s.id == id) {
        if s.phase != code {
            s.phase = code;
            s.phase_at = now;
        }
        s.progress = prog;
        s.seen = now;
        s.at = at;
        return;
    }
    // First heard (or a late joiner): its clock starts where the server's is. A landing already well
    // past isn't replayed.
    let woke = now.checked_sub(std::time::Duration::from_secs_f32(age)).unwrap_or(now);
    fx.shrines.push(ShrineFx {
        id: id.to_string(),
        at,
        kind: kind.to_string(),
        phase: code,
        progress: prog,
        shown: prog,
        woke,
        phase_at: now,
        seen: now,
        fall,
        landed: age > fall + 2.0,
        cued: false,
        seed,
        ring,
    });
}

/// The crown: the column surges gold and launches, a shell bursts out, its chime (and a jolt, in `frame`).
fn shrine_crown(fx: &mut Fx, at: DVec3, kind: &str, seed: u32) {
    let now = Instant::now();
    fx.effects.push(Effect {
        shape: Shape::Shrine,
        a: at,
        b: at + DVec3::Y * SHRINE_TOP,
        radius: 5.0,
        rgb: type_rgb(kind),
        seed,
        start: now,
        secs: SHRINE_CROWN_SECS,
        caster: String::new(),
        strength: 1.0,
        style: type_index(kind),
        flags: 4,
        look: [1.0, 0.0, 0.0, 0.0],
    });
    if fx.effects.len() > 64 {
        fx.effects.remove(0);
    }
    for (sound, vol, pitch, after) in [
        ("minecraft:item.totem.use", 1.6, 0.9, 0.0),
        ("minecraft:block.beacon.power_select", 1.5, 0.8, 0.2),
        ("minecraft:block.amethyst_block.resonate", 1.4, 0.7, 0.45),
    ] {
        if fx.later.len() < 16 {
            fx.later.push((sound, at + DVec3::Y, vol, pitch, now + std::time::Duration::from_secs_f32(after)));
        }
    }
    fx.crowned.push(at);
}

/// Each frame: shrines the server stopped naming go; a meteor rumbles as it nears; its landing plays
/// once (the release cinematic, booms by distance tier delayed d/343, a jolt that arrives with the
/// ground's thump, five times faster than the air).
fn shrines_frame(fx: &mut Fx, eye: Option<DVec3>, now: Instant) {
    fx.shrines.retain(|s| s.seen.elapsed().as_secs_f32() < SHRINE_STALE);
    let mut rumble: f32 = 0.0;
    let mut land = Vec::new();
    let mut cue = Vec::new();
    for s in &mut fx.shrines {
        // The drawn count eases toward the server's (it updates twice a second).
        s.shown += (s.progress - s.shown) * 0.15;
        let t = s.woke_secs();
        if t < s.fall {
            let k = (t / s.fall).clamp(0.0, 1.0);
            if let Some(eye) = eye {
                rumble = rumble.max(arrival::rumble(&s.kind, k) * far_falloff(eye.distance(s.at)));
            }
        }
        if !s.landed && t >= s.fall {
            s.landed = true;
            land.push(s.clone());
        }
        if !s.cued && t < s.fall {
            s.cued = true;
            cue.push(s.clone());
        }
    }
    fx.rumble = rumble;
    // A build-up's sounds and jolts, timed from its waking: each delayed by its distance and heard by
    // distance tier (arrival.rs); what is already past for a late joiner is skipped.
    for s in cue {
        let Some(eye) = eye else { continue };
        let woke = s.woke;
        for c in arrival::cues(&s.kind, s.fall) {
            if c.t < s.woke_secs() {
                continue;
            }
            let src = match c.from {
                arrival::From::Up(h) => s.at + DVec3::Y * h,
                arrival::From::Focus => arrival::focus(&s.kind, s.at, s.seed, c.t, s.fall),
            };
            let d = eye.distance(src);
            let (heard, vol) = if d < 120.0 {
                (true, c.vol)
            } else if d < 300.0 {
                (c.far, c.vol * 0.6)
            } else {
                (c.big, c.vol * 0.4)
            };
            let when = woke + std::time::Duration::from_secs_f32(c.t);
            if heard && fx.timed.len() < 48 {
                let from = if d < 64.0 { src } else { eye + (src - eye).normalize_or_zero() * 12.0 };
                fx.timed.push((c.sound, from, vol, c.pitch, when + std::time::Duration::from_secs_f64(d / SOUND_SPEED)));
            }
            if c.jolt > 0.0 {
                fx.jolts.push((when + std::time::Duration::from_secs_f64(d / (SOUND_SPEED * 5.0)), 0.5, c.jolt * far_falloff(d)));
            }
        }
    }
    for s in land {
        let at = s.at + DVec3::Y * 0.5;
        // The Z-Move release staging, as a burst: a sky pillar slamming down, the dome, shock rings,
        // scorch and embers (its own one flash obeys `/capifx flash off`).
        fx.effects.push(Effect {
            shape: Shape::ZRelease,
            a: at,
            b: at,
            radius: 6.0,
            rgb: type_rgb(&s.kind),
            seed: s.seed,
            start: now,
            secs: Z_RELEASE_SECS,
            caster: format!("shrine:{}", s.id),
            strength: 1.0,
            style: type_index(&s.kind),
            flags: 2,
            look: [0.0; 4],
        });
        if fx.effects.len() > 64 {
            fx.effects.remove(0);
        }
        let Some(eye) = eye else { continue };
        let d = eye.distance(at);
        let arrive = now + std::time::Duration::from_secs_f64(d / SOUND_SPEED);
        // Far away the sound plays a little way off toward the shrine, so it is heard at all.
        let from = if d < 64.0 { at } else { eye + (at - eye).normalize_or_zero() * 12.0 };
        let mut booms = landing_booms(d, &s.kind);
        // The type's own hit, 0.38 s in (between the thunder and the cry), near and mid only.
        if let (Some((sound, vol, pitch)), true) = (arrival::climax_sound(&s.kind), d < 220.0) {
            booms.push((sound, if d < 64.0 { vol } else { vol * 0.5 }, pitch, 0.38));
        }
        for (sound, vol, pitch, after) in booms {
            if fx.timed.len() < 64 {
                fx.timed.push((sound, from, vol, pitch, arrive + std::time::Duration::from_secs_f32(after)));
            }
        }
        let thump = now + std::time::Duration::from_secs_f64(d / (SOUND_SPEED * 5.0));
        fx.jolts.push((thump, 1.8, 0.95 * far_falloff(d)));
    }
    for at in std::mem::take(&mut fx.crowned) {
        if let Some(eye) = eye {
            let d = eye.distance(at);
            fx.jolts.push((now + std::time::Duration::from_secs_f64(d / (SOUND_SPEED * 5.0)), 1.2, 0.55 * far_falloff(d)));
        }
    }
    fx.jolts.retain(|(at, secs, _)| now < *at || now.duration_since(*at).as_secs_f32() < *secs);
    if fx.jolts.len() > 64 {
        fx.jolts.remove(0);
    }
}

/// The stacked jolts' trauma now: each `s (1 - u)^2`, combined as the root of the sum of squares.
fn jolts_now(jolts: &[(Instant, f32, f32)], now: Instant) -> f32 {
    jolts
        .iter()
        .filter(|(at, ..)| now >= *at)
        .map(|(at, secs, s)| {
            let u = (now.duration_since(*at).as_secs_f32() / secs.max(0.01)).clamp(0.0, 1.0);
            let v = s * (1.0 - u) * (1.0 - u);
            v * v
        })
        .sum::<f32>()
        .sqrt()
}

/// All the camera's shake now (0-1): a hit on this player, Z-Moves, a meteor's rumble and the
/// stacked jolts, added as the root of the sum of squares and capped.
fn shake_total(hit: f32, z: f32, rumble: f32, jolts: f32) -> f32 {
    (hit * hit + z * z + rumble * rumble + jolts * jolts).sqrt().min(1.0)
}

/// The shrine effects for the pass, as `pack` lays them out (see the shader's `shrinefx`).
fn shrine_effects(fx: &Fx) -> Vec<Effect> {
    fx.shrines
        .iter()
        .map(|s| {
            let t = s.woke_secs();
            let falling = t < s.fall;
            Effect {
                shape: Shape::Shrine,
                a: s.at,
                b: if falling { s.focus() } else { s.at + DVec3::Y * SHRINE_TOP },
                radius: s.ring,
                rgb: type_rgb(&s.kind),
                seed: s.seed,
                start: Instant::now(),
                secs: 1.0,
                caster: String::new(),
                strength: 1.0,
                style: type_index(&s.kind),
                flags: if falling { 0 } else { s.phase.max(1) },
                look: [s.shown, t, s.phase_at.elapsed().as_secs_f32(), s.fall],
            }
        })
        .collect()
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
    trainer::frame(&mut fx);
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
    fx.zarmed.retain(|_, z| z.live());
    // Z-Move booms play once the sound has had time to reach the camera; releases shake it.
    let eye = host.client_state().and_then(|s| s.eye_position).map(DVec3::from);
    let now = Instant::now();
    let later = std::mem::take(&mut fx.later);
    for (sound, at, vol, pitch, when) in later {
        let delay = eye.map_or(0.0, |e| e.distance(at) / SOUND_SPEED);
        if now >= when + std::time::Duration::from_secs_f64(delay.min(1.0)) {
            host.play_local_sound(sound, at.to_array(), vol, pitch, 0);
        } else {
            fx.later.push((sound, at, vol, pitch, when));
        }
    }
    let timed = std::mem::take(&mut fx.timed);
    for (sound, at, vol, pitch, when) in timed {
        if now >= when {
            host.play_local_sound(sound, at.to_array(), vol, pitch, 0);
        } else {
            fx.timed.push((sound, at, vol, pitch, when));
        }
    }
    shrines_frame(&mut fx, eye, now);
    fx.zshake = match eye {
        Some(eye) => fx
            .effects
            .iter()
            .filter(|e| e.shape == Shape::ZRelease)
            .map(|e| z_shake(false, e.age(), eye.distance(e.b)) * if e.flags == Z_HEAL_FORM { Z_HEAL_SHAKE } else { 1.0 })
            .fold(0.0, f32::max),
        None => 0.0,
    };
    // A capsule opening: it floats ahead of the camera, plays its sounds on time and shakes.
    if fx.capsule.as_ref().is_some_and(|c| c.t() >= CAPSULE_END) {
        fx.capsule = None;
    }
    let flat = capsule_flat_for(fx.low, RENDER.off.load(Ordering::Relaxed));
    if let Some(c) = fx.capsule.as_mut() {
        let t = c.t();
        // The camera players see through (the shoulder camera for a Pokémon), else the eye.
        let view = host
            .camera()
            .map(|cam| (DVec3::from(cam.position), cam.yaw, cam.pitch))
            .or_else(|| host.client_state().and_then(|s| s.eye_position.map(|e| (DVec3::from(e), s.yaw, s.pitch))));
        if let Some((from, yaw, pitch)) = view {
            c.at = Some(capsule_place(from, yaw, pitch));
            c.yaw = yaw;
        }
        let (due, n) = capsule_sounds(c.played, t, c.tier);
        c.played = n;
        if let Some(at) = c.at {
            for (sound, vol, pitch) in due {
                host.play_local_sound(sound, at.to_array(), vol, pitch, 0);
            }
        }
        // No camera shake for a capsule the effects pass isn't drawing.
        let shake = if flat { 0.0 } else { capsule_shake(t, c.tier) };
        fx.zshake = fx.zshake.max(shake);
    }
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
        fx.no_flash = host.setting(crate::MOD_ID, "flash").as_deref() == Some("off");
    }
    // A beam, a burst and a fire coat stay with their caster (who keeps moving after the server
    // placed them), and so does a guard shell.
    let follow: Vec<String> = fx
        .effects
        .iter()
        .filter(|e| matches!(e.shape, Shape::Coat | Shape::Beam | Shape::Burst | Shape::Charge | Shape::Aura | Shape::ZGather))
        .map(|e| e.caster.clone())
        .chain(fx.guarding.iter().cloned())
        .chain(crate::play::sig::auras().into_iter().map(|(n, ..)| n))
        .chain(trainer::follows(&fx))
        .collect();
    fx.bodies.clear();
    drop(fx);
    // This client's name every frame, also with nothing to follow: a hit on it (`own_view`).
    if follow.is_empty() {
        let me = crate::play::CLIENT.lock().unwrap().my_name.clone();
        FX.lock().unwrap().me = me;
        return;
    }
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
        let p = DVec3::from(e.previous_position).lerp(DVec3::from(e.position), partial);
        // A wild (not a player) is followed by its entity id, `e:<id>`, at the engine's height for it.
        let by_id = format!("e:{}", e.id);
        if follow.contains(&by_id) && e.alive {
            bodies.insert(by_id, (p, f64::from(trainer::npc_height(e.id).unwrap_or(e.height)).clamp(0.3, 6.0)));
        }
        let Some(name) = e.player_name else { continue };
        if !follow.contains(&name) {
            continue;
        }
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
    // A Z-Move's build-up shakes cameras near the caster harder as it charges.
    if let Some(eye) = eye {
        let gather = fx
            .effects
            .iter()
            .filter(|e| e.shape == Shape::ZGather)
            .filter_map(|e| bodies.get(&e.caster).map(|(feet, _)| z_shake(true, e.age(), eye.distance(*feet)) * if e.flags == Z_HEAL_GATHER { Z_HEAL_SHAKE } else { 1.0 }))
            .fold(0.0, f32::max);
        fx.zshake = fx.zshake.max(gather);
    }
    fx.bodies = bodies;
    fx.me = me;
}

/// The throw a trainer is lining up, for the faint line of it (`catch.rs`); None when not aiming.
pub fn aim(line: Option<([f64; 3], [f64; 3], bool, f64)>) {
    trainer::set_aim(&mut FX.lock().unwrap(), line.map(|(a, b, locked, h)| (DVec3::from(a), DVec3::from(b), locked, h)));
}

/// Whether the effects pass can't draw (the frame format or the driver refused it): what it
/// would have drawn needs a stand-in.
pub fn pass_off() -> bool {
    RENDER.off.load(Ordering::Relaxed)
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
    // `/capifx flash off|on`: whole-picture flashes (a Z-Move's release, a shrine's crown) off or on.
    if command.split_whitespace().nth(1) == Some("flash") {
        let off = match command.split_whitespace().nth(2) {
            Some("off") => true,
            Some("on") => false,
            _ => !FX.lock().unwrap().no_flash,
        };
        host.set_setting(crate::MOD_ID, "flash", if off { "off" } else { "on" });
        FX.lock().unwrap().no_flash = off;
        host.client_action_bar(horizon_client::mod_api::Text::literal(if off { "Capipimon FX: flashes off" } else { "Capipimon FX: flashes on" }), false);
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

/// Smooth value noise in -1..1 (one octave), its own lattice per `salt`.
fn vnoise(x: f32, salt: f32) -> f32 {
    let h = |i: f32| {
        let v = ((i * 127.1 + salt * 311.7).sin() * 43758.547).fract();
        if v < 0.0 { v + 1.0 } else { v }
    };
    let i = x.floor();
    let f = x - i;
    let s = f * f * (3.0 - 2.0 * f);
    (h(i) + (h(i + 1.0) - h(i)) * s) * 2.0 - 1.0
}

/// The shake's motion for trauma `amt` at time `t` on one axis: three octaves of value noise (2.1,
/// 6.3 and 13 Hz), the low ones slowing as trauma rises so a big hit feels heavy rather than buzzy
/// (the model Orbital Laser documents; written again here).
fn shake_motion(t: f32, amt: f32, salt: f32) -> f32 {
    let slow = 1.0 - 0.35 * amt.clamp(0.0, 1.0);
    (0.55 * vnoise(t * 2.1 * slow, salt) + 0.35 * vnoise(t * 6.3 * slow, salt + 17.0) + 0.1 * vnoise(t * 13.0, salt + 41.0)) * 1.5
}

/// Camera shake when this player is hit, near a Z-Move, under a falling meteor and at its landing.
pub fn camera_shake(event: &mut CameraShakeEvent<'_>) {
    let t = {
        let fx = FX.lock().unwrap();
        shake_total(trauma_now(&fx), fx.zshake, fx.rumble, jolts_now(&fx.jolts, Instant::now()))
    };
    if t <= 0.0 {
        return;
    }
    let k = t * t;
    let s = EPOCH.elapsed().as_secs_f32();
    event.shake.pitch += k * 3.5 * shake_motion(s, t, 0.0);
    event.shake.yaw += k * 3.5 * shake_motion(s, t, 11.0);
    event.shake.roll += k * 2.5 * shake_motion(s, t, 23.0);
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
    // Z-Moves first: the build-up lights its caster more as it charges; the release floods the
    // impact with its colour, then cools.
    let mut lights = 0;
    // Shrines: a meteor lights the ground as it comes down; an awake shrine glows in its type's
    // colour, warming to gold as it is captured, red while contested; a crown floods it gold.
    let secs = EPOCH.elapsed().as_secs_f32();
    for s in fx.shrines.iter().take(2) {
        let t = s.woke_secs();
        let rgb = type_rgb(&s.kind);
        let light = if t < s.fall {
            let k = (t / s.fall).clamp(0.0, 1.0);
            let m = s.focus();
            NativeColoredLight { at: m.to_array(), rgb: rgb_u32(rgb), radius: 10.0 + 6.0 * k, intensity: 0.3 + 0.6 * k }
        } else {
            let gold = Vec3::new(1.0, 0.74, 0.2);
            let war = s.phase == 3;
            let c = if war { rgb.lerp(Vec3::new(1.0, 0.1, 0.06), 0.5 + 0.3 * (secs * 8.0).sin()) } else { rgb.lerp(gold, s.shown * 0.85) };
            let breathe = 0.5 + 0.5 * (secs * 1.3 + s.seed as f32).sin();
            let flare = (-(t - s.fall) * 1.2).exp();
            NativeColoredLight { at: (s.at + DVec3::Y * 2.0).to_array(), rgb: rgb_u32(c), radius: 9.0 + 4.0 * s.shown + 6.0 * flare, intensity: 0.4 + 0.08 * breathe + 0.3 * s.shown + 0.5 * flare }
        };
        event.lights.push(light);
        lights += 1;
    }
    for e in fx.effects.iter().filter(|e| e.shape == Shape::Shrine).take(1) {
        let k = (1.0 - e.age()).clamp(0.0, 1.0);
        event.lights.push(NativeColoredLight { at: (e.a + DVec3::Y * 3.0).to_array(), rgb: rgb_u32(Vec3::new(1.0, 0.78, 0.3)), radius: 8.0 + 10.0 * k, intensity: 0.3 + 0.7 * k * k });
        lights += 1;
    }
    // A capsule opening lights the place in its rarity's colour, brighter as it charges, then a
    // flood at the burst that cools.
    if let Some((c, at)) = fx.capsule.as_ref().and_then(|c| c.at.map(|at| (c, at))) {
        let t = c.t();
        let (radius, intensity) = if t < CAPSULE_BURST {
            let ch = (t / CAPSULE_BURST).clamp(0.0, 1.0);
            (3.0 + 3.0 * ch, 0.2 + 0.4 * ch)
        } else {
            let k = (-(t - CAPSULE_BURST) * 1.2).exp();
            (5.0 + (6.0 + 3.0 * c.tier as f32) * k, 0.25 + 0.75 * k)
        };
        event.lights.push(NativeColoredLight { at: at.to_array(), rgb: rgb_u32(c.rgb), radius, intensity });
        lights += 1;
    }
    for e in fx.effects.iter().filter(|e| matches!(e.shape, Shape::ZGather | Shape::ZRelease)).take(2) {
        let age = e.age().clamp(0.0, 1.0);
        let (at, radius, intensity) = if e.shape == Shape::ZGather {
            let Some((feet, h)) = fx.bodies.get(&e.caster) else { continue };
            (*feet + DVec3::Y * *h * 0.5, 4.0 + 5.0 * age, 0.3 + 0.5 * age)
        } else {
            let t = age * Z_RELEASE_SECS;
            let k = if t < 0.1 { t / 0.1 } else { (-(t - 0.1) * 1.6).exp() };
            (e.b + DVec3::Y, 7.0 + 7.0 * k, (0.15 + 0.85 * k) * ((1.0 - age) * 4.0).min(1.0))
        };
        event.lights.push(NativeColoredLight { at: at.to_array(), rgb: rgb_u32(e.rgb), radius, intensity });
        lights += 1;
    }
    for e in fx.effects.iter().filter(|e| !matches!(e.shape, Shape::Coat | Shape::Guard | Shape::Warn | Shape::Aura | Shape::ZGather | Shape::ZRelease | Shape::Shrine)).take(8usize.saturating_sub(lights)) {
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
        lights += 1;
    }
    drop(fx);
    // Legendary auras light their wearer and the ground round them, in whatever room the moves
    // left (8 in all), the nearest few. Seen from where the camera is now.
    let mut near = crate::play::cosmetics::draw::legends();
    near.retain(|(_, b)| f64::from(b.dist) < LEGEND_FADE.1);
    near.sort_by(|a, b| a.1.dist.total_cmp(&b.1.dist));
    for (_, b) in near.iter().take(LEGEND_LIGHTS.min(8usize.saturating_sub(lights))) {
        event.lights.push(legend_light(b, secs));
    }
}

fn rgb_vec(c: u32) -> Vec3 {
    Vec3::new(((c >> 16) & 0xFF) as f32, ((c >> 8) & 0xFF) as f32, (c & 0xFF) as f32) / 255.0
}

/// The legendary auras the effects pass draws, nearest first, at most `LEGEND_MAX`, fading out
/// with distance (`LEGEND_FADE`). `secs`: the clock the shader runs on. The flare's age rides in
/// `secs` (the shader's p.w), the accent colour and body height in `look`.
fn legend_effects(camera: DVec3, secs: f32) -> Vec<Effect> {
    let mut all = crate::play::cosmetics::draw::legends();
    all.sort_by(|a, b| a.1.feet.distance(camera).total_cmp(&b.1.feet.distance(camera)));
    all.into_iter()
        .filter_map(|(name, b)| {
            let mid = b.feet + DVec3::Y * f64::from(b.fit.height) * 0.5;
            let d = mid.distance(camera);
            let fade = 1.0 - ((d - LEGEND_FADE.0) / (LEGEND_FADE.1 - LEGEND_FADE.0)).clamp(0.0, 1.0);
            (fade > 0.0).then(|| {
                let acc = rgb_vec(b.look.accent);
                Effect {
                    shape: Shape::Legend,
                    a: mid,
                    b: b.feet,
                    radius: b.fit.ring,
                    rgb: rgb_vec(b.tint),
                    seed: b.seed,
                    start: Instant::now(),
                    secs: crate::play::cosmetics::draw::flare(b.seed, secs).0,
                    caster: name,
                    strength: fade as f32,
                    style: b.look.theme,
                    flags: 0,
                    look: [acc.x, acc.y, acc.z, b.fit.height],
                }
            })
        })
        .take(LEGEND_MAX)
        .collect()
}

/// A legendary aura's light: a steady glow that breathes, swelling at each flare.
fn legend_light(b: &crate::play::cosmetics::draw::LegendBody, secs: f32) -> NativeColoredLight {
    let (t, _) = crate::play::cosmetics::draw::flare(b.seed, secs);
    let flare = (-t * 3.0).exp();
    let breathe = 0.5 + 0.5 * (secs * 1.7 + (b.seed % 31) as f32).sin();
    // The aura's tint, pulled toward the accent at a flare.
    let rgb = rgb_vec(b.tint).lerp(rgb_vec(b.look.accent), 0.25 + 0.5 * flare);
    let at = b.feet + DVec3::Y * f64::from(b.fit.height) * 0.4;
    NativeColoredLight {
        at: at.to_array(),
        rgb: rgb_u32(rgb),
        radius: (3.5 + 1.8 * b.fit.k + 2.5 * flare).min(14.0),
        intensity: 0.28 + 0.08 * breathe + 0.45 * flare,
    }
}

/// Whether this client sees an effect from inside or right behind it: its own wind-up, status
/// aura, fire coat, self burst and lunge, and a hit on it (a hit's `caster` is who was hit). Those
/// keep the old added light, no solid body (docs/fx/MOVE_FX_VISIBILITY.md): the third-person camera
/// sits up to 11 blocks back for a big species, too far for the shader's 3-8 block fade, and a
/// solid one covered its own Pokemon. Not its beams and cones (they reach out where it aims) or a
/// delayed strike (`<name>~<tag>`, it lands elsewhere), nor a Z-Move (its own cinematic).
/// `my_h` is this client's model height: a burst no bigger than its self-buff puff (server
/// `(height * 0.6).max(0.8)`) sits on its body; a wider one (Earthquake, an ally heal) keeps its
/// solid rim, which shows its reach.
fn own_view(e: &Effect, me: Option<&str>, my_h: f64) -> bool {
    // A meteor's comet is a dash falling 14 blocks onto where it lands: not round its caster.
    let comet = e.shape == Shape::Dash && e.a.y - e.b.y > 8.0;
    let wide = e.shape == Shape::Burst && f64::from(e.radius) > (my_h * 0.6).max(0.8) + 0.1;
    me.is_some_and(|me| e.caster == me)
        && !comet
        && !wide
        && matches!(e.shape, Shape::Coat | Shape::Charge | Shape::Aura | Shape::Burst | Shape::Dash | Shape::Hit)
}

/// Added to the shape number for an `own_view` effect (the shader takes it off again).
const OWN_VIEW: f32 = 64.0;

/// One effect as the shader reads it (4 vec4s).
fn pack(e: &Effect, a: Vec3, b: Vec3, age: f32, own: bool, out: &mut Vec<f32>) {
    out.extend_from_slice(&[a.x, a.y, a.z, e.radius]);
    out.extend_from_slice(&[b.x, b.y, b.z, e.shape as u32 as f32 + if own { OWN_VIEW } else { 0.0 }]);
    out.extend_from_slice(&[e.rgb.x, e.rgb.y, e.rgb.z, e.strength]);
    out.extend_from_slice(&[age.clamp(0.0, 1.0), (e.seed % 997) as f32, (e.style + 32 * e.flags) as f32, e.secs]);
    out.extend_from_slice(&e.look);
}

/// Floats per effect in the uniform buffer (5 vec4s).
const FX_FLOATS: usize = 20;

/// What to draw this frame, camera-relative, nearest first; empty when nothing is live. `fwd`: the
/// way the camera looks (a capsule opening floats ahead of it).
fn snapshot(camera: DVec3, fwd: Option<DVec3>) -> Vec<f32> {
    let mut fx = FX.lock().unwrap();
    let low = fx.low;
    // Where each followed player is now; this client's own feet come from this frame's camera.
    let local = crate::play::local_feet().map(DVec3::from);
    let kit = {
        let (bodies, me) = (fx.bodies.clone(), fx.me.clone());
        let body = move |name: &str| -> Option<(DVec3, f64)> {
            let (feet, h) = *bodies.get(name)?;
            Some((if me.as_deref() == Some(name) { local.unwrap_or(feet) } else { feet }, h))
        };
        trainer::effects(&mut fx, &body)
    };
    if low && kit.is_empty() {
        return Vec::new();
    }
    let shrines = shrine_effects(&fx);
    let fx = fx;
    let mut live: Vec<(f64, Effect)> = Vec::new();
    let mut add = |e: Effect| {
        if low && !kit_shape(e.shape) && !(e.shape == Shape::Aura && e.flags == AURA_HEAL) {
            return;
        }
        // Distance from the camera to the effect's segment.
        let ab = e.b - e.a;
        let s = if ab.length_squared() > 1e-9 { ((camera - e.a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0) } else { 0.0 };
        let d = (e.a + ab * s).distance(camera) - f64::from(e.radius);
        // A capsule opening always gets a slot, and a Z-Move is seen from further off and gets
        // one before ordinary effects (they sort first).
        if e.shape == Shape::Capsule {
            live.push((-2000.0, e));
        } else if e.shape == Shape::Shrine {
            // Seen from across the map; at most two at once, before any move.
            if d < SHRINE_CULL {
                live.push((d - 1800.0, e));
            }
        } else if kit_shape(e.shape) {
            if d < Z_CULL {
                live.push((d - 1500.0, e));
            }
        } else if matches!(e.shape, Shape::ZGather | Shape::ZRelease) {
            if d < Z_CULL {
                live.push((d - 1000.0, e));
            }
        } else if d < CULL {
            live.push((d, e));
        }
    };
    let body = |name: &str| -> Option<(DVec3, f64)> {
        let (feet, h) = *fx.bodies.get(name)?;
        let feet = if fx.me.as_deref() == Some(name) { local.unwrap_or(feet) } else { feet };
        Some((feet, h))
    };
    if let Some(e) = fx.capsule.as_ref().and_then(|c| capsule_drawn(c, camera, fwd)) {
        add(e);
    }
    for e in kit {
        add(e);
    }
    for e in shrines {
        add(e);
    }
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
            (Shape::ZGather, Some((feet, h))) => {
                let c = feet + DVec3::Y * h * 0.55;
                add(Effect { a: c, b: c, radius: (h * 0.6).clamp(0.5, 3.0) as f32, ..e.clone() });
            }
            (Shape::ZGather, None) => {}
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
    // Legendary auras last: they only fill slots no move wants (sorted after every move).
    let secs = EPOCH.elapsed().as_secs_f32();
    for e in legend_effects(camera, secs) {
        live.push((e.a.distance(camera) + 5000.0, e));
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
    let me = fx.me.clone();
    let my_h = me.as_deref().and_then(|m| fx.bodies.get(m)).map_or(1.8, |b| b.1);
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
        let age = if e.shape == Shape::Guard || (e.shape == Shape::Aura && !matches!(e.flags, 6 | AURA_HEAL)) { 0.5 } else if e.shape == Shape::Shrine && e.flags < 4 { 0.0 } else { e.age() };
        pack(e, a, b, age, own_view(e, me.as_deref(), my_h), &mut out);
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
            let b = arc(uv, 0.88, 0.085, lead) + arc(uv, 1.14, 0.085, lead + 0.25);
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
            let b = arc(uv, 1.0, 0.09, lead) + arc(vec2<f32>(-uv.x, uv.y) + vec2<f32>(0.25, 0.0), 1.0, 0.09, lead);
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

// ---- Z-Move cinematic (docs/fx/ZMOVE_CINEMATIC.md) ----

// Type-coloured light capped like `one`'s (hue kept, brightest channel near 1.3), plus white.
fn zfinish(col: vec3<f32>, glow: f32, core: f32, white: f32, dark: f32, s: f32) -> vec4<f32> {
    let lum = dot(col, vec3<f32>(0.299, 0.587, 0.114));
    let cs = max(mix(vec3<f32>(lum), col, 1.45), vec3<f32>(0.0));
    var tinted = cs * glow * 1.2 + mix(cs, vec3<f32>(1.0), 0.25) * core * 1.8;
    let peak = max(tinted.r, max(tinted.g, tinted.b));
    tinted = tinted * min(1.0, 1.3 / max(peak, 1e-4));
    return vec4<f32>((tinted + vec3<f32>(1.0) * min(white, 2.5)) * s, clamp(dark * s, 0.0, 0.85));
}

// A type's character along a Z-Move's pillar or line: x along it (blocks), ang round it.
// Returns (width scale, brightness mask, dimming, white-hot share).
fn zflav(ty: i32, x: f32, ang: f32, time: f32, seed: f32) -> vec4<f32> {
    var w = 1.0;
    var m = 1.0;
    var dk = 0.0;
    var wh = 0.45;
    if (ty == 1) {
        // Fire: a roaring, flickering column.
        let n = noise1(x * 1.5 - time * 12.0 + seed);
        w = 0.9 + 0.35 * n;
        m = 0.85 + 0.35 * n;
        wh = 0.35;
    } else if (ty == 2) {
        // Water: a twisting torrent with white water.
        m = 0.55 + 0.7 * pow(0.5 + 0.5 * sin(ang * 2.0 + x * 1.8 - time * 10.0), 2.0);
        wh = 0.6;
    } else if (ty == 3) {
        // Electric: thin and blinding, surging along it 30 times a second (smooth along it: hard
        // segment steps drew blocks).
        w = 0.65;
        m = 0.45 + 1.1 * smoothstep(0.2, 0.6, 0.5 + 0.5 * noise1(x * 0.9 + floor(time * 30.0) * 3.7 + seed));
        wh = 0.75;
    } else if (ty == 4) {
        // Grass: bands of leaf-light spiralling up.
        m = 0.5 + 0.8 * step(0.5, fract(x * 1.2 - time * 4.0 + ang * 0.32));
        wh = 0.2;
    } else if (ty == 5) {
        // Ice: hard, with glinting facets.
        w = 0.9;
        m = 0.85 + 0.9 * pow(hash(floor(x * 3.0) + seed), 6.0);
        wh = 0.6;
    } else if (ty == 6) {
        // Fighting: heavy shock rings pounding down it.
        w = 1.15;
        m = 0.6 + 1.0 * exp(-sq((fract(x * 0.35 - time * 2.5) - 0.5) * 6.0));
        wh = 0.4;
    } else if (ty == 7) {
        // Poison: thick, bulging, a little murky.
        w = 0.8 + 0.5 * (0.5 + 0.5 * noise1(x * 0.8 - time * 2.0 + seed));
        dk = 0.15;
        wh = 0.2;
    } else if (ty == 8) {
        // Ground: broad, gritty, dusty.
        w = 1.3;
        m = 0.6 + 0.4 * hash(floor(x * 3.0) + floor(time * 12.0) + seed);
        dk = 0.3;
        wh = 0.1;
    } else if (ty == 9) {
        // Flying: spiralling air.
        m = 0.4 + 0.9 * pow(0.5 + 0.5 * sin(ang * 3.0 + x * 1.2 - time * 9.0), 3.0);
        wh = 0.5;
    } else if (ty == 10) {
        // Psychic: pulses rolling along it.
        m = 0.45 + 1.0 * exp(-sq((fract(x * 0.6 - time * 1.6) - 0.5) * 8.0));
        wh = 0.3;
    } else if (ty == 11) {
        // Bug: a swarm of specks.
        m = 0.2 + 1.2 * step(0.45, hash(floor(x * 4.0) + floor(ang * 3.0) + floor(time * 10.0) + seed));
        wh = 0.25;
    } else if (ty == 12) {
        // Rock: chunky segments.
        w = 1.2;
        m = 0.5 + 0.5 * step(0.3, fract(x * 0.8));
        dk = 0.2;
        wh = 0.15;
    } else if (ty == 13) {
        // Ghost: a wide, drifting column that darkens the world round it.
        w = 1.3;
        m = 0.7 + 0.3 * noise1(x * 0.7 - time * 1.8 + seed);
        dk = 0.5;
        wh = 0.1;
    } else if (ty == 14) {
        // Dragon: a twisting double helix.
        m = 0.4 + 0.9 * pow(0.5 + 0.5 * sin(ang * 2.0 - x * 1.4 + time * 8.0), 4.0);
        wh = 0.4;
    } else if (ty == 15) {
        // Dark: ink that drains the light.
        w = 1.2;
        dk = 0.6;
        wh = 0.0;
    } else if (ty == 16) {
        // Steel: mirror-bright with sharp glints.
        m = 0.9 + 1.5 * pow(hash(floor(x * 2.0) + floor(time * 20.0) + seed), 10.0);
        wh = 0.8;
    } else if (ty == 17) {
        // Fairy: soft, with twinkles.
        m = 0.6 + 1.4 * pow(hash(floor(x * 5.0) + floor(ang * 4.0) + floor(time * 10.0) + seed), 8.0);
        wh = 0.4;
    }
    return vec4<f32>(w, m, dk, wh);
}

// Glowing cracks running out from a point on the ground: dd is the distance from it (blocks),
// ang the angle round it, reach how far they have run. 1 on a crack.
fn zcracks(dd: f32, ang: f32, reach: f32, seed: f32) -> f32 {
    if (reach <= 0.0 || dd > reach) { return 0.0; }
    let tau = 6.28318;
    let a1 = ang / tau * 9.0 + 0.35 * noise1(dd * 1.7 + seed) + hash(seed);
    let d1 = abs(fract(a1) - 0.5) * tau / 9.0 * dd;
    var c = exp(-sq(d1 / 0.045)) * (1.0 - smoothstep(reach * 0.6, reach, dd));
    // Shorter branches between them.
    let a2 = ang / tau * 17.0 + 0.5 * noise1(dd * 2.9 + seed + 13.0) + hash(seed + 5.0);
    let d2 = abs(fract(a2) - 0.5) * tau / 17.0 * dd;
    let br = exp(-sq(d2 / 0.035)) * (1.0 - smoothstep(reach * 0.35, reach * 0.55, dd)) * step(0.4, hash(floor(a2) + seed));
    c = max(c, br);
    return c;
}

// ---- Legendary auras (cosmetics_draw.rs `legend`, docs/COSMETICS.md "Legendary auras") ----

// One glyph in a rune band cell: u across the cell, v across the band (0-1); h picks its strokes.
fn rune(u: f32, v: f32, h: f32) -> f32 {
    let m = smoothstep(0.08, 0.2, u) * (1.0 - smoothstep(0.8, 0.92, u)) * smoothstep(0.05, 0.18, v) * (1.0 - smoothstep(0.82, 0.95, v));
    var g = ln(u - 0.5, 0.07) * step(0.3, fract(h * 7.0));
    g = max(g, ln(v - 0.5, 0.08) * step(0.5, fract(h * 13.0)));
    g = max(g, ln(u - v, 0.07) * step(0.55, fract(h * 17.0)));
    g = max(g, ln(u + v - 1.0, 0.07) * step(0.6, fract(h * 23.0)));
    g = max(g, ln(length(vec2<f32>(u, v) - vec2<f32>(0.5)) - 0.22, 0.06) * step(0.75, fract(h * 29.0)));
    return g * m;
}

// The type's pattern on the floor inside the rune ring: p is the ground point over the ring's
// reach (|p| = rn, 1 at the outer ring). Returns (light in the tint, light in the accent, dimming).
fn legend_floor(ty: i32, p: vec2<f32>, rn: f32, ang: f32, time: f32, seed: f32) -> vec3<f32> {
    let tau = 6.2831853;
    let inside = 1.0 - smoothstep(0.7, 0.78, rn);
    var g = 0.0;
    var a = 0.0;
    var dk = 0.0;
    if (ty == 1) {
        // Fire: tongues of flame licking out from the middle, a white-hot heart.
        let fl = noise1(ang * 2.5 + time * 2.5 + seed) * 0.5 + 0.5;
        g = pow(max(0.0, 1.0 - rn / (0.4 + 0.38 * fl)), 1.4) * (0.75 + 0.25 * noise1(time * 9.0 + ang * 3.0));
        a = pow(max(0.0, 1.0 - rn / 0.28), 2.0) * 0.9;
    } else if (ty == 2) {
        // Water: ripples rolling out from under it.
        g = ln(fract(rn * 3.0 - time * 0.5) - 0.5, 0.07) * inside * 0.7;
        a = ln(fract(rn * 3.0 - time * 0.5 + 0.5) - 0.5, 0.035) * inside * 0.45;
    } else if (ty == 3) {
        // Electric: jagged arcs jumping between the middle and the ring.
        let k = (ang / tau) * 7.0 + time * 0.3;
        let jag = noise1(rn * 18.0 + floor(time * 14.0) * 3.1 + floor(k) * 5.0) * 0.18;
        let lit = step(0.45, hash(floor(k) + floor(time * 9.0) + seed));
        g = ln(fract(k + jag) - 0.5, 0.05) * lit * inside * smoothstep(0.08, 0.2, rn);
        a = g * 0.9 + ln(rn, 0.12) * (0.6 + 0.4 * hash(floor(time * 20.0)));
    } else if (ty == 4) {
        // Grass: vines spiralling out, buds along them.
        let sp = fract(ang / tau * 3.0 + rn * 1.4 - time * 0.08);
        g = ln(sp - 0.5, 0.07) * inside * 0.75;
        a = ln(fract(rn * 6.0 - time * 0.3) - 0.5, 0.12) * g;
    } else if (ty == 5) {
        // Ice: a six-spoked frost star inside a hexagon of facets, twinkling.
        let a6 = abs(fract(ang / tau * 6.0 + 0.5) - 0.5) * tau / 6.0;
        let spoke = ln(rn * sin(a6), 0.022) * inside * smoothstep(0.05, 0.15, rn);
        let hex = ln(rn * cos(a6) - 0.5, 0.018) + ln(rn * cos(a6) - 0.3, 0.012) * 0.6;
        g = (spoke + hex) * 0.8;
        a = spoke * pow(0.5 + 0.5 * sin(time * 3.0 - rn * 12.0), 3.0);
    } else if (ty == 6) {
        // Fighting: shock rings pounding out on a beat.
        let beat = fract(time * 1.1);
        g = ln(rn - (0.12 + 0.62 * beat), 0.035) * (1.0 - beat) * 1.3 * inside;
        a = exp(-rn * rn / 0.02) * pow(1.0 - beat, 3.0);
    } else if (ty == 7) {
        // Poison: blotches bubbling up over a sickly pool.
        let q = p * 5.0;
        let c = floor(q);
        let h = hash(c.x * 7.1 + c.y * 13.7 + seed);
        let fr = fract(q) - 0.5;
        let grow = 0.5 + 0.5 * sin(time * 2.0 + h * 6.0);
        g = smoothstep(0.32, 0.05, length(fr)) * step(0.45, h) * grow * inside * 0.6;
        a = ln(length(fr) - 0.32 * grow, 0.03) * step(0.45, h) * inside * 0.7;
        dk = inside * 0.15;
    } else if (ty == 8) {
        // Ground: cracks glowing with magma.
        let cr = zcracks(rn, ang, 0.78, seed);
        g = cr * 0.9;
        a = cr * (0.4 + 0.6 * (0.5 + 0.5 * sin(time * 2.0 + rn * 6.0)));
        dk = inside * 0.12;
    } else if (ty == 9) {
        // Flying (the tide): arms of wind sweeping round, spray where they pass.
        let s = sin(ang * 3.0 + log(rn + 0.05) * 5.0 - time * 2.2);
        g = pow(max(s, 0.0), 8.0) * inside * 0.75;
        a = ln(fract(rn * 2.5 - time * 0.4) - 0.5, 0.04) * inside * 0.35;
    } else if (ty == 10) {
        // Psychic: two rings of nodes turning against each other, a wave breathing out.
        let line = ln(rn - 0.45, 0.012) * 0.6 + ln(rn - 0.62, 0.01) * 0.4;
        let wv = fract(time * 0.4);
        g = line + ln(rn - wv * 0.75, 0.03) * (1.0 - wv) * 0.7;
        a = ln(rn - 0.45, 0.035) * pow(0.5 + 0.5 * cos((ang + time * 0.6) * 6.0), 14.0)
          + ln(rn - 0.62, 0.03) * pow(0.5 + 0.5 * cos((ang - time * 0.45) * 8.0), 18.0);
        a = a * 1.4;
    } else if (ty == 11) {
        // Bug: a honeycomb whose cells light up in turn.
        let q = p * 7.0;
        let l1 = abs(fract(q.x) - 0.5);
        let l2 = abs(fract(0.5 * q.x + 0.866 * q.y) - 0.5);
        let l3 = abs(fract(0.5 * q.x - 0.866 * q.y) - 0.5);
        g = ln(min(l1, min(l2, l3)), 0.05) * inside * 0.45;
        let c = floor(q * 0.5);
        a = step(0.8, hash(c.x * 3.3 + c.y * 9.1 + floor(time * 3.0))) * inside * 0.35;
    } else if (ty == 12) {
        // Rock: a ring of heavy stones, cracks between them.
        let seg = fract(ang / tau * 10.0);
        let stone = smoothstep(0.1, 0.2, seg) * (1.0 - smoothstep(0.8, 0.9, seg)) * ln(rn - 0.58, 0.07);
        g = stone * (0.45 + 0.4 * hash(floor(ang / tau * 10.0) + seed));
        a = zcracks(rn, ang + 0.3, 0.55, seed + 3.0) * 0.6;
        dk = inside * 0.1;
    } else if (ty == 13) {
        // Ghost: mist swirling over a pool of shadow, wisps curling in.
        let m = noise1(ang * 3.0 + rn * 6.0 - time * 0.8 + seed) * 0.5 + 0.5;
        g = m * m * inside * 0.35 * (1.0 - rn);
        a = pow(max(sin(ang * 4.0 + rn * 8.0 - time * 1.5), 0.0), 10.0) * inside * 0.55;
        dk = inside * 0.35 * (0.7 + 0.3 * m);
    } else if (ty == 14) {
        // Dragon: a double helix of scales coiling round.
        let s1 = fract(ang / tau * 2.0 + rn * 1.2 - time * 0.25);
        g = ln(s1 - 0.5, 0.06) * inside * 0.8;
        a = ln(s1 - 0.5, 0.02) * ln(fract(rn * 8.0 - time) - 0.5, 0.15) * inside;
    } else if (ty == 15) {
        // Dark: an ink pool drinking the light, crimson veins through it.
        let n = noise1(ang * 2.0 + time * 0.5 + seed) * 0.5 + 0.5;
        dk = inside * (0.4 + 0.15 * n);
        a = zcracks(rn, ang, 0.78, seed + 7.0) * (0.6 + 0.4 * sin(time * 1.5 + rn * 5.0));
    } else if (ty == 16) {
        // Steel: a toothed ring like a gear, a glint sweeping round.
        let teeth = step(0.5, fract(ang / tau * 16.0 + time * 0.05)) * ln(rn - 0.62, 0.035);
        g = teeth * 0.6 + ln(rn - 0.55, 0.012) * 0.6;
        a = pow(max(cos(ang - time * 1.5), 0.0), 40.0) * inside * smoothstep(0.1, 0.3, rn) * 0.9;
    } else if (ty == 17) {
        // Fairy: a flower of light, sparkles winking over it.
        g = pow(0.5 + 0.5 * cos(ang * 5.0 + time * 0.3), 6.0) * ln(rn - 0.4, 0.09) * 0.6 + (1.0 - rn) * inside * 0.15;
        let q = p * 6.0;
        let c = floor(q);
        let h = hash(c.x * 5.3 + c.y * 11.9 + seed);
        a = ln(length(fract(q) - 0.5), 0.07) * step(0.6, h) * pow(0.5 + 0.5 * sin(time * 4.0 + h * 20.0), 4.0) * inside;
    } else if (ty == 18) {
        // Starlight: a slowly turning sky of stars, a faint glow under them.
        let q = rot2(p, time * 0.1) * 6.0;
        let c = floor(q);
        let h = hash(c.x * 4.7 + c.y * 12.3 + seed);
        let off = vec2<f32>(hash(h * 3.0), hash(h * 5.0)) * 0.6 + 0.2;
        a = ln(length(fract(q) - off), 0.05) * step(0.55, h) * (0.5 + 0.5 * sin(time * 3.0 + h * 30.0)) * inside * 1.2;
        g = (1.0 - rn) * inside * 0.2;
    } else {
        // Normal: a sun of clean rays round a bright ring.
        g = pow(0.5 + 0.5 * cos(ang * 12.0 + time * 0.3), 8.0) * (1.0 - smoothstep(0.2, 0.75, rn)) * smoothstep(0.08, 0.2, rn) * 0.55;
        a = ln(rn - 0.18, 0.02) * 0.9;
    }
    return vec3<f32>(g, a, dk);
}

// A legendary aura round a body (a: its middle, r: the ring's radius round the feet; b: the feet;
// colour: tint and strength; p: (-, seed, theme, seconds since its last flare); m: accent colour
// and body height). Light only, plus a little dimming for ghost, dark and poison: a rune ring and
// the type's pattern on the ground, a sheath of light rising off the body, three slow pillars
// round the ring, a soft core, and every few seconds a flare in the type's manner.
fn legend(f: Fx, rd: vec3<f32>, tmax: f32, time: f32) -> vec4<f32> {
    let tau = 6.2831853;
    let feet = f.b.xyz;
    let R = max(f.a.w, 0.3);
    let H = max(f.m.w, 0.4);
    let seed = f.p.y;
    let ty = i32(f.p.z + 0.5) % 32;
    let ft = f.p.w;
    let col = f.color.rgb;
    let acc = f.m.xyz;
    let reach = R * 1.45 + 0.25;
    let up = vec3<f32>(0.0, 1.0, 0.0);
    var on_ground = false;
    var gp = vec3<f32>(0.0);
    if (tmax < 9000.0) {
        gp = rd * tmax;
        on_ground = abs(gp.y - feet.y) < 0.45 + R * 0.08 && length((gp - feet).xz) < reach * 1.25 + 1.6 * min(ft, 1.5) * reach;
    }
    let top = feet + up * (H * 1.5 + 0.3);
    let q0 = ray_seg(rd, tmax, feet, top);
    let d0 = length(q0.xyz - mix(feet, top, q0.w));
    // Nothing near the body, its column or the ground round it: skip the rest.
    if (d0 > reach + 1.0 && !on_ground && !(ty == 3 && ft < 0.35)) { return vec4<f32>(0.0); }
    var glow = 0.0;
    var accg = 0.0;
    var white = 0.0;
    var dark = 0.0;
    let flare = exp(-ft * 3.0);
    // The camera stands inside the column (its own first-person view): the body's layers give way.
    let cam_d = length(feet.xz);
    let open = smoothstep(R * 0.7, R * 2.2, cam_d);
    if (on_ground) {
        let gv = (gp - feet).xz;
        let dd = length(gv);
        let ang = atan2(gv.y, gv.x);
        let band = 1.0 - smoothstep(0.12, 0.3 + R * 0.08, abs(gp.y - feet.y));
        let rn = dd / reach;
        let lw = 0.03 + 0.01 * R;
        // Two rings and the runes turning slowly between them.
        var ring = ln(dd - reach, lw) * 1.1 + ln(dd - reach * 0.8, lw * 0.8) * 0.8;
        if (ty == 3) { ring = ring * (0.6 + 0.6 * step(0.35, hash(floor(ang * 6.0 + time * 9.0) + seed))); }
        let cells = floor(10.0 + 6.0 * R);
        let cu = (ang / tau + 0.5 + time * 0.03) * cells;
        let v = (rn - 0.82) / 0.15;
        let lit = 0.55 + 0.45 * sin(time * 2.0 + floor(cu) * 1.7);
        let runes = rune(fract(cu), v, hash(floor(cu) + seed)) * lit;
        let fl = legend_floor(ty, gv / reach, rn, ang, time, seed);
        // The light it throws pooled on the ground.
        let pool = exp(-rn * rn * 2.2) * (0.16 + 0.12 * flare);
        glow = glow + (ring + fl.x + pool) * band;
        accg = accg + (runes * 1.1 + fl.y) * band;
        dark = max(dark, fl.z * band);
        // Every flare: a shock ring rolling out over the ground.
        if (ft < 1.6) {
            let rr = reach * (0.3 + 1.6 * ft);
            let shock = ln(dd - rr, 0.06 + 0.12 * ft * R) * exp(-ft * 2.2);
            if (ty == 15 || ty == 13) {
                // Dark and ghost pull in instead, and the ground drinks the light.
                let ri = reach * max(1.5 - 2.2 * ft, 0.0);
                accg = accg + ln(dd - ri, 0.08 * R + 0.05) * exp(-ft * 2.0) * band;
                dark = max(dark, (1.0 - smoothstep(reach * 0.6, reach * 1.2, dd)) * 0.3 * exp(-ft * 1.5) * band);
            } else {
                accg = accg + shock * 1.2 * band;
            }
            if (ty == 5) {
                // Ice: frost spikes shoot out.
                accg = accg + pow(0.5 + 0.5 * cos(ang * 8.0 + seed), 24.0) * (1.0 - smoothstep(reach * (0.3 + 1.2 * ft), reach * (0.4 + 1.2 * ft), dd)) * exp(-ft * 2.5) * band;
            }
        }
    }
    if (d0 < reach + 1.0) {
        // A sheath of light rising off the body: thin streaks in columns, faint, the type's colour.
        let c = feet + up * (H * 0.5);
        let side = normalize(cross(up, rd) + vec3<f32>(1e-4, 0.0, 0.0));
        let tc = clamp(dot(c, rd), 0.0, tmax);
        let v = rd * tc - c;
        let rb = clamp(R * 0.85, 0.3, H * 1.2);
        let sx = dot(v, side) / rb;
        let sy = v.y / (H * 0.5);
        let cx = sx * 3.5;
        let cell = floor(cx);
        let colm = ln(fract(cx) - 0.5, 0.09) * step(0.35, hash(cell * 1.31 + seed));
        let rise = pow(fract(sy * 0.5 - time * 0.55 + hash(cell * 1.7 + seed)), 5.0);
        let span = (1.0 - smoothstep(0.9, 1.3, abs(sx))) * smoothstep(-1.2, -0.8, sy) * (1.0 - smoothstep(1.2, 1.9, sy));
        // Only where the body's middle is in front of the scene: on the ground before it, the clamped
        // closest point sat on the floor and drew the streaks onto it.
        let vis = select(0.0, 1.0, dot(c, rd) < tmax + rb * 0.5);
        glow = glow + colm * rise * span * vis * (0.32 + 0.4 * flare) * open;
        // A soft core, breathing.
        let hb = ray_ball(rd, tmax, c, rb);
        let breathe = 0.75 + 0.25 * sin(time * 1.7 + seed);
        glow = glow + exp(-sq(hb.x / (rb * 1.1))) * 0.09 * breathe * hb.y * open;
        white = white + exp(-sq(hb.x / (rb * 0.5))) * 0.25 * flare * hb.y * open;
        if (ty == 10) {
            // Psychic: a halo widening round the body at each flare.
            accg = accg + ln(hb.x - rb * (1.0 + 1.6 * ft), 0.06 * rb + 0.03) * exp(-ft * 2.5) * open;
        }
        // Three pillars of light turning slowly round the ring, swelling at a flare.
        let pw = 0.025 + 0.012 * R;
        let pm = 0.9 + 2.0 * flare;
        for (var i = 0; i < 3; i = i + 1) {
            let fi = f32(i);
            let pa = time * 0.45 + fi * 2.0944 + seed;
            let base = feet + vec3<f32>(cos(pa) * R * 1.05, 0.0, sin(pa) * R * 1.05);
            let ph = H * (0.75 + 0.35 * sin(time * 1.3 + fi * 2.1)) + 0.3;
            let pt = base + up * ph;
            let q = ray_seg(rd, tmax, base, pt);
            let d = length(q.xyz - mix(base, pt, q.w));
            let fall = pow(1.0 - q.w, 1.5);
            var shimmer = 1.0;
            if (ty == 1 || ty == 8) { shimmer = 0.6 + 0.6 * noise1(q.w * 6.0 - time * 8.0 + fi * 3.0); }
            glow = glow + (ln(d, pw) * 0.8 + ln(d, pw * 5.0) * 0.16) * fall * pm * shimmer * open;
            accg = accg + ln(d, pw * 0.45) * fall * 0.5 * pm * open;
        }
        if ((ty == 1 || ty == 8) && ft < 1.3) {
            // Fire and ground: a column of flame roars up the body at a flare.
            let ct = feet + up * (H * 1.9 + 0.5);
            let q = ray_seg(rd, tmax, feet, ct);
            let d = length(q.xyz - mix(feet, ct, q.w));
            let w = rb * (0.55 - 0.35 * q.w);
            let tongue = 0.55 + 0.45 * noise1(q.w * 7.0 - time * 12.0 + seed);
            accg = accg + ln(d, w) * tongue * exp(-ft * 2.2) * (1.0 - q.w) * 1.3 * open;
        }
    }
    if (ty == 3 && ft < 0.35) {
        // Electric: a bolt drops out of the sky onto the ring, flickering twice.
        let k = floor(time - ft + 0.5) + seed;
        let ba = hash(k) * tau;
        let hit = feet + vec3<f32>(cos(ba) * R, 0.0, sin(ba) * R);
        var p0 = hit + up * (H * 2.5 + 5.0) + vec3<f32>(hash(k + 1.0) - 0.5, 0.0, hash(k + 2.0) - 0.5) * 2.0;
        var bolt = 0.0;
        for (var j = 0; j < 4; j = j + 1) {
            let fj = f32(j + 1);
            var p1 = mix(p0, hit, fj / 4.0);
            if (j < 3) { p1 = p1 + vec3<f32>(hash(k + fj * 3.1) - 0.5, 0.0, hash(k + fj * 5.7) - 0.5) * (0.6 + 0.3 * R); }
            let q = ray_seg(rd, tmax, p0, p1);
            let d = length(q.xyz - mix(p0, p1, q.w));
            bolt = max(bolt, ln(d, 0.05) + ln(d, 0.4) * 0.2);
            p0 = p1;
        }
        let blink = step(ft, 0.07) + step(0.13, ft) * step(ft, 0.22);
        white = white + bolt * blink * 1.6;
        accg = accg + bolt * blink;
    }
    let s = f.color.a;
    let lum = dot(col, vec3<f32>(0.299, 0.587, 0.114));
    let cs = max(mix(vec3<f32>(lum), col, 1.3), vec3<f32>(0.0));
    var light = cs * glow + acc * accg;
    let peak = max(light.r, max(light.g, light.b));
    light = light * min(1.0, 1.6 / max(peak, 1e-4));
    return vec4<f32>((light + vec3<f32>(min(white, 2.0))) * s, clamp(dark * s, 0.0, 0.6));
}

// A Z-Move's build-up on its caster (a: body centre, r: body radius, age 0-1 of the wind-up):
// an aura rising off it, rings of energy closing in on a core that swells, and the ground under
// its feet cracking and glowing.
fn zgather(f: Fx, rd: vec3<f32>, tmax: f32, time: f32) -> vec4<f32> {
    let c = f.a.xyz;
    let r = max(f.a.w, 0.3);
    let g = clamp(f.p.x, 0.0, 1.0);
    let seed = f.p.y;
    let fl = i32(floor(f.p.z / 32.0 + 0.001));
    let ty = i32(f.p.z + 0.5) - fl * 32;
    var col = f.color.rgb;
    if (ty == 15) { col = mix(col, vec3<f32>(0.9, 0.12, 0.15), 0.6); }
    // A healing Z-Move (flags 1): leaf-green light, and the ground blooms instead of cracking.
    let heal = fl == 1;
    if (heal) { col = zheal_col(f.color.rgb); }
    var glow = 0.0;
    var core = 0.0;
    var white = 0.0;
    var dark = 0.0;
    let feet_y = c.y - r * 0.92;
    let reach = r * 2.8;
    let tc = clamp(dot(c, rd), 0.0, tmax);
    let v = rd * tc - c;
    var on_ground = false;
    var gp = vec3<f32>(0.0);
    if (tmax < 9000.0) {
        gp = rd * tmax;
        on_ground = abs(gp.y - feet_y) < 1.0 && length((gp - c).xz) < reach;
    }
    // Nothing near the body, the aura over it or the ground round it: skip the rest.
    if (length(v) > r * 5.0 + 1.0 && !on_ground) { return vec4<f32>(0.0); }
    let hb = ray_ball(rd, tmax, c, r * 2.8);
    let side = normalize(cross(vec3<f32>(0.0, 1.0, 0.0), rd) + vec3<f32>(1e-4, 0.0, 0.0));
    let sx = dot(v, side) / r;
    let sy = v.y / r;
    // Aura: streaks rising off the body in columns, taller and faster as it charges.
    let cx = sx * 3.0;
    let cell = floor(cx);
    let top = 1.3 + 2.4 * g;
    let colm = ln(fract(cx) - 0.5, 0.1) * step(0.3, hash(cell * 1.31 + seed)) * (0.6 + 0.4 * hash(cell + floor(time * 15.0)));
    let rise = pow(fract(sy * 0.45 - time * (1.2 + 2.5 * g) + hash(cell * 1.7)), 6.0);
    let span = (1.0 - smoothstep(1.0, 1.5, abs(sx))) * smoothstep(-1.3, -0.9, sy) * (1.0 - smoothstep(top - 0.6, top, sy));
    glow = glow + (colm * rise * (0.35 + 1.1 * g) + exp(-sq(sx / 1.1)) * 0.12 * g) * span * hb.y;
    // Rings of energy closing in on the body, over and over.
    let ang = atan2(v.y, dot(v, side));
    for (var i = 0; i < 3; i = i + 1) {
        let ph = fract(time * 1.3 + f32(i) / 3.0);
        let rr = r * (2.8 - 2.4 * ph);
        let spark = 0.4 + 0.9 * pow(hash(floor(ang * 4.0 + time * 6.0 + f32(i) * 7.0) + seed), 3.0);
        glow = glow + ln(hb.x - rr, r * 0.05) * ph * spark * (0.4 + 0.8 * g) * hb.y;
    }
    // The core it gathers into.
    let k = exp(-sq(hb.x / (r * (0.12 + 0.25 * g)))) * (0.3 + 1.6 * g * g) * hb.y;
    core = core + k;
    white = white + k * 0.5 * g;
    if (ty == 3) { glow = glow * (0.6 + 0.8 * hash(floor(time * 30.0) + seed)); }
    if ((ty == 13 || ty == 15) && !heal) { dark = max(dark, exp(-sq(hb.x / (r * 1.5))) * 0.35 * g * hb.y); }
    if (on_ground && heal) {
        // Rings of light breathing out over the ground, wider as it charges.
        let gv = gp - c;
        let dd = length(gv.xz);
        let band = 1.0 - smoothstep(0.3, 1.0, abs(gp.y - feet_y));
        let out = reach * (0.3 + 0.7 * g);
        let ring = ln(dd - out * fract(time * 0.8), 0.1) + ln(dd - out * fract(time * 0.8 + 0.5), 0.1);
        glow = glow + (ring * (0.5 + 0.6 * g) * (1.0 - smoothstep(out * 0.7, out, dd)) + exp(-sq(dd / (reach * 0.5))) * 0.35 * g) * band;
    }
    // The ground under its feet cracks open and glows.
    if (on_ground && !heal) {
        let gv = gp - c;
        let dd = length(gv.xz);
        let gang = atan2(gv.z, gv.x);
        let band = 1.0 - smoothstep(0.3, 1.0, abs(gp.y - feet_y));
        let cr = zcracks(dd, gang, reach * (0.15 + 0.85 * g), seed);
        glow = glow + (cr * (0.6 + 1.2 * g) + exp(-sq(dd / (reach * 0.45))) * 0.3 * g) * band;
        white = white + cr * 0.3 * g * band;
        dark = max(dark, (1.0 - smoothstep(reach * 0.5, reach, dd)) * 0.2 * g * band);
    }
    return zfinish(col, glow, core, white, dark, f.color.a);
}

// A healing Z-Move's light: the type's colour pulled most of the way to leaf green.
fn zheal_col(c: vec3<f32>) -> vec3<f32> {
    return mix(c, vec3<f32>(0.25, 1.0, 0.45), 0.65);
}

// A healing Z-Move's release (flags 6; b: the caster's middle, r: how far it reaches). Nothing
// slams down or scorches: a column of light rises from the caster, soft rings roll out over the
// ground to the edge of its reach, which stays marked while the ground glows, and motes drift up.
fn zheal(f: Fx, rd: vec3<f32>, tmax: f32, time: f32) -> vec4<f32> {
    let b = f.b.xyz;
    let reach = max(f.a.w, 2.0);
    let age = clamp(f.p.x, 0.0, 1.0);
    let seed = f.p.y;
    let tt = age * f.p.w;
    let col = zheal_col(f.color.rgb);
    var glow = 0.0;
    var core = 0.0;
    var white = 0.0;
    let fade = 1.0 - smoothstep(0.7, 1.0, age);
    // A camera standing in the column (the caster's own) isn't blinded by it.
    let near = 0.3 + 0.7 * smoothstep(2.0, 9.0, length(b.xz));
    let flash = exp(-sq((tt - 0.13) / 0.08)) / (1.0 + length(b) / 24.0);
    glow = glow + flash * 0.12;
    let top = 40.0 * smoothstep(0.04, 0.5, tt);
    let cenv = smoothstep(0.03, 0.12, tt) * (1.0 - smoothstep(1.0, 2.1, tt));
    if (cenv > 0.0 && top > 0.1) {
        let pbot = b - vec3<f32>(0.0, 1.0, 0.0);
        let ptop = b + vec3<f32>(0.0, top, 0.0);
        let q = ray_seg(rd, tmax, pbot, ptop);
        let axis = pbot + (ptop - pbot) * q.w;
        let d = length(q.xyz - axis);
        let w = 1.5;
        let x = axis.y - b.y;
        let tip = 1.0 - smoothstep(top - 8.0, top, x);
        let bands = 0.7 + 0.6 * exp(-sq((fract(x * 0.15 - time * 1.3) - 0.5) * 6.0));
        let k = cenv * tip * near;
        core = core + exp(-sq(d / (w * 0.35))) * 0.55 * k;
        glow = glow + (exp(-sq(d / w)) * 0.6 * bands + exp(-d / (w * 2.0)) * 0.3) * k;
        white = white + exp(-sq(d / (w * 0.2))) * 0.2 * k;
    }
    if (tmax < 9000.0) {
        let gv = rd * tmax - b;
        let hd = length(gv.xz);
        if (gv.y > -4.0 && gv.y < 1.5 && hd < reach * 1.1) {
            let band = smoothstep(-4.0, -3.0, gv.y) * (1.0 - smoothstep(0.8, 1.5, gv.y));
            for (var i = 0; i < 3; i = i + 1) {
                let k = clamp((tt - 0.1 - 0.35 * f32(i)) / 1.3, 0.0, 1.0);
                if (k > 0.0 && k < 1.0) {
                    let rr = reach * (1.0 - sq(1.0 - k));
                    let lv = (1.0 - k) * band;
                    glow = glow + ln(hd - rr, 0.25 + 0.3 * k) * 0.9 * lv;
                    white = white + ln(hd - rr, 0.08) * 0.3 * lv;
                }
            }
            // The healed ground glows, brightest at the caster, with the edge of its reach marked.
            let lit = smoothstep(0.1, 0.4, tt) * fade * band;
            let petals = 0.75 + 0.25 * sin(atan2(gv.z, gv.x) * 8.0 + hd * 1.5 - time * 1.5 + seed);
            glow = glow + ((1.0 - smoothstep(reach * 0.15, reach, hd)) * 0.3 * petals + ln(hd - reach, 0.12) * 0.7) * lit;
        }
    }
    // Motes drifting up all over it.
    let menv = smoothstep(0.15, 0.5, tt) * fade;
    let tcb = dot(b, rd);
    if (menv > 0.0 && tcb > 0.0 && tcb < tmax + 2.0) {
        let side = normalize(cross(vec3<f32>(0.0, 1.0, 0.0), rd) + vec3<f32>(1e-4, 0.0, 0.0));
        let v = rd * tcb - b;
        let sx = dot(v, side);
        let sy = v.y;
        if (abs(sx) < reach && sy > -1.5 && sy < 9.0) {
            for (var l = 0; l < 2; l = l + 1) {
                let sc = 1.6 + f32(l) * 1.1;
                let cx = sx * sc + f32(l) * 0.5;
                let h1 = hash(floor(cx) * 1.73 + seed + f32(l) * 19.0);
                let speed = 0.8 + 1.2 * hash(floor(cx) * 2.9 + f32(l));
                let yy = fract(h1 + tt * speed * 0.14) * 8.0 - 1.0;
                let xx = (fract(cx) - 0.5 + 0.25 * sin(tt * 2.0 + h1 * 6.0)) / sc;
                let de = length(vec2<f32>(xx, sy - yy));
                let lit = step(0.3, h1) * menv * (1.0 - smoothstep(5.0, 7.0, yy));
                glow = glow + exp(-sq(de / 0.1)) * lit * 1.3;
                core = core + exp(-sq(de / 0.04)) * lit * 0.7;
            }
        }
    }
    return zfinish(col, glow, core, white, 0.0, f.color.a);
}

// A Z-Move's release and aftermath (a: where it left, b: where it lands, r: the move's radius,
// flags: 1 beam, 2 burst, 3 dash, 4 cone, 5 ground; p.w: how long it lasts). Over ~2.6 s: the
// energy implodes into the caster, one flash, a pillar slams down from the sky onto the impact
// with the move's own line blazing, shock rings race out over the ground, the ground is left
// scorched and cracked white-hot, then it cools while embers rise.
fn zrelease(f: Fx, rd: vec3<f32>, tmax: f32, time: f32) -> vec4<f32> {
    let a = f.a.xyz;
    let b = f.b.xyz;
    let r = f.a.w;
    let age = clamp(f.p.x, 0.0, 1.0);
    let seed = f.p.y;
    let fl = i32(floor(f.p.z / 32.0 + 0.001));
    let ty = i32(f.p.z + 0.5) - fl * 32;
    let tt = age * f.p.w;
    if (fl == 6) { return zheal(f, rd, tmax, time); }
    var col = f.color.rgb;
    if (ty == 15) { col = mix(col, vec3<f32>(0.9, 0.12, 0.15), 0.6); }
    var glow = 0.0;
    var core = 0.0;
    var white = 0.0;
    var dark = 0.0;
    let fade = 1.0 - smoothstep(0.75, 1.0, age);
    // A narrow move's pillar and rings still read from the far end of its own beam.
    // A zone or a nuke stands a pillar as wide as its area is big.
    var pmax = 2.4;
    if (fl == 2 || fl == 5) { pmax = 5.0; }
    let pw = clamp(r * 0.5, 1.1, pmax);
    // The dome (0.55 of this) swells to the area really hit.
    let big = clamp(r * 1.6 + 3.0, 7.0, 34.0);
    // How far the camera stands from the pillar's axis (the camera is the origin). A caster whose
    // Z-Move lands on itself (a burst, a field) stands inside it: closest-approach shading then
    // whites out its whole picture, and the smoke dims it in blocks that follow the depth behind.
    let camd = length(b.xz);
    let inside = 0.3 + 0.7 * smoothstep(pw * 2.0, pw * 6.0, camd);
    // One flash over the whole picture as it lands (a single pulse, never a strobe), fainter
    // the further away it is.
    // Off with `/capifx flash off` (u.info.z).
    let flash = exp(-sq((tt - 0.13) / 0.06)) / (1.0 + length(b) / 24.0) * (1.0 - clamp(u.info.z, 0.0, 1.0));
    glow = glow + flash * 0.22;
    white = white + flash * 0.08;
    // Implosion: the gathered energy collapses into where it leaves.
    if (tt < 0.16) {
        let k = clamp(tt / 0.13, 0.0, 1.0);
        let off = 1.0 - smoothstep(0.13, 0.16, tt);
        let hi = ray_ball(rd, tmax, a, pw * 2.5);
        let rr = pw * 2.5 * (1.0 - k);
        glow = glow + ln(hi.x - rr, pw * 0.12) * (0.5 + k) * hi.y * off;
        core = core + exp(-sq(hi.x / (pw * (0.2 + 0.5 * k)))) * k * 1.5 * hi.y * off;
    }
    // The sky pillar: its foot falls from 40 blocks up onto the impact, bulges as it hits, then
    // thins and fades.
    let penv = smoothstep(0.08, 0.12, tt) * (1.0 - smoothstep(0.85, 1.7, tt));
    let smoke = smoothstep(0.5, 1.1, tt) * fade;
    if (penv > 0.0 || smoke > 0.0) {
        let ptop = b + vec3<f32>(0.0, 40.0, 0.0);
        let pbot = b + vec3<f32>(0.0, 40.0 * (1.0 - smoothstep(0.09, 0.19, tt)) - 1.5, 0.0);
        let q = ray_seg(rd, tmax, pbot, ptop);
        let axis = pbot + (ptop - pbot) * q.w;
        let off = q.xyz - axis;
        let d = length(off);
        let wbase = pw * (1.0 + 0.7 * exp(-sq((tt - 0.2) / 0.08))) * (1.0 - 0.45 * smoothstep(0.5, 1.5, tt));
        let x = axis.y - b.y;
        // Smoke left hanging where it stood, widening and drifting up (it dims, it doesn't glow).
        if (smoke > 0.0) {
            let sw = pw * 1.8 * (1.0 + (tt - 0.5) * 0.6);
            let puff = 0.6 + 0.4 * noise1(x * 0.5 - tt * 1.5 + seed) + 0.25 * noise1(atan2(off.z, off.x) * 2.0 + x * 0.9 + seed);
            dark = max(dark, exp(-sq(d / sw)) * puff * smoke * 0.35 * (1.0 - smoothstep(4.0, 16.0, x)) * smoothstep(pw * 2.0, pw * 5.0, camd));
        }
        if (penv > 0.0 && d < wbase * 9.0) {
            let fv = zflav(ty, x, atan2(off.z, off.x), time, seed);
            let w = wbase * fv.x;
            let m = fv.y * penv;
            // The type's pattern drives the core only: on the wide halo a pattern made of steps
            // (electric segments, grass bands) cut the soft glow into hard-edged blocks.
            core = core + exp(-sq(d / (w * 0.32))) * m * 1.5 * inside;
            glow = glow + (exp(-sq(d / (w * 0.9))) * 0.5 * m + (exp(-d / wbase) * 0.6 + exp(-sq(d / (wbase * 2.5))) * 0.2) * penv) * inside;
            white = white + exp(-sq(d / (w * 0.18))) * fv.w * m * 1.2 * inside;
            dark = max(dark, exp(-sq(d / (w * 1.6))) * fv.z * penv);
            // Bright bands slide down its skin.
            glow = glow + ln(d - w * 1.15, w * 0.12) * exp(-sq((fract(x * 0.12 + time * 1.5) - 0.5) * 9.0)) * 0.6 * penv;
        }
    }
    // The move's own line (beam, dash, cone), fat and white-hot, two strands winding round it.
    if (fl == 1 || fl == 3 || fl == 4) {
        let ab = b - a;
        let len = length(ab);
        let lenv = smoothstep(0.02, 0.08, tt) * (1.0 - smoothstep(0.7, 1.3, tt));
        if (len > 0.05 && lenv > 0.0) {
            let head = clamp((tt - 0.02) / 0.08, 0.0, 1.0);
            let bb = a + ab * head;
            let q = ray_seg(rd, tmax, a, bb);
            let c0 = a + (bb - a) * q.w;
            let d0 = length(q.xyz - c0);
            var lw = clamp(r * 0.9, 0.5, 3.0);
            if (fl == 4) { lw = max(r * 0.6, 0.6) * (0.3 + 0.7 * q.w); }
            if (d0 < lw * 7.0) {
                let dir = ab / len;
                let u1 = perp(dir);
                let u2 = cross(dir, u1);
                let x = q.w * len * head;
                let fv = zflav(ty, x, 0.0, time, seed);
                let w = lw * fv.x;
                core = core + exp(-sq(d0 / (w * 0.35))) * fv.y * lenv * 1.3;
                glow = glow + (exp(-sq(d0 / (w * 0.9))) * 0.35 * fv.y + exp(-d0 / lw) * 0.45) * lenv;
                white = white + exp(-sq(d0 / (w * 0.2))) * fv.w * lenv;
                dark = max(dark, exp(-sq(d0 / (w * 1.4))) * fv.z * lenv);
                let phi = x * 1.6 - time * 10.0 + seed;
                let s1 = c0 + (u1 * cos(phi) + u2 * sin(phi)) * w * 0.95;
                let s2 = c0 - (u1 * cos(phi) + u2 * sin(phi)) * w * 0.95;
                let ds = min(length(q.xyz - s1), length(q.xyz - s2));
                glow = glow + exp(-sq(ds / (w * 0.18))) * 0.9 * lenv;
            }
        }
    }
    // A burst or ground strike: a blazing dome swells over the point.
    if (fl == 2 || fl == 5) {
        let k = clamp((tt - 0.1) / 0.45, 0.0, 1.0);
        let rr = max(big * 0.55 * (1.0 - pow(1.0 - k, 3.0)), 0.1);
        let hd = ray_ball(rd, tmax, b, rr);
        let denv = smoothstep(0.1, 0.14, tt) * (1.0 - smoothstep(0.35, 0.9, tt));
        glow = glow + (ln(hd.x - rr, max(rr * 0.07, 0.08)) * 0.9 + step(hd.x, rr) * 0.12) * denv * hd.y * inside;
        core = core + ln(hd.x - rr, max(rr * 0.03, 0.05)) * 0.4 * denv * hd.y * inside;
    }
    // The air shock: a thin bright shell racing out.
    let se = clamp((tt - 0.12) / 0.7, 0.0, 1.0);
    if (se > 0.0 && se < 1.0) {
        let rs = big * (1.0 - sq(1.0 - se));
        let hs = ray_ball(rd, tmax, b, rs);
        white = white + ln(hs.x - rs, max(rs * 0.025, 0.05)) * 0.35 * (1.0 - se) * hs.y;
    }
    // On the ground round the impact: shock rings, then scorch and cracks cooling from white-hot.
    if (tmax < 9000.0) {
        let p = rd * tmax;
        let gv = p - b;
        let dy = gv.y;
        let hd = length(gv.xz);
        if (dy > -4.0 && dy < 1.5 && hd < big * 1.05) {
            let band = smoothstep(-4.0, -3.0, dy) * (1.0 - smoothstep(0.8, 1.5, dy));
            for (var i = 0; i < 3; i = i + 1) {
                let k = clamp((tt - 0.12 - 0.2 * f32(i)) / 1.1, 0.0, 1.0);
                if (k > 0.0 && k < 1.0) {
                    let rr = big * (1.0 - pow(1.0 - k, 2.5));
                    let rw = 0.18 + 0.35 * k;
                    let lv = sq(1.0 - k) * (1.0 - 0.25 * f32(i)) * band;
                    glow = glow + ln(hd - rr, rw) * 1.1 * lv;
                    white = white + ln(hd - rr, rw * 0.3) * 0.5 * lv;
                }
            }
            let heat = exp(-max(tt - 0.15, 0.0) * 1.4) * smoothstep(0.1, 0.16, tt);
            let reach = big * 0.55 * smoothstep(0.1, 0.35, tt);
            let cr = zcracks(hd, atan2(gv.z, gv.x), reach, seed);
            glow = glow + cr * (0.35 + 1.4 * heat) * band * fade;
            white = white + cr * heat * 0.6 * band;
            core = core + exp(-sq(hd / (pw * 1.3))) * heat * 1.2 * band;
            dark = max(dark, (1.0 - smoothstep(reach * 0.6, reach * 1.05, hd)) * 0.45 * smoothstep(0.12, 0.3, tt) * fade * band);
        }
    }
    // Embers drifting up off the impact once the blast has passed.
    let eenv = smoothstep(0.25, 0.6, tt) * fade;
    let tcb = dot(b, rd);
    if (eenv > 0.0 && tcb > 0.0 && tcb < tmax + 2.0) {
        let side = normalize(cross(vec3<f32>(0.0, 1.0, 0.0), rd) + vec3<f32>(1e-4, 0.0, 0.0));
        let v = rd * tcb - b;
        let sx = dot(v, side);
        let sy = v.y;
        if (abs(sx) < big * 0.6 && sy > -1.5 && sy < 12.0) {
            for (var l = 0; l < 2; l = l + 1) {
                let sc = 2.6 + f32(l) * 1.5;
                let cx = sx * sc + f32(l) * 0.5;
                let h1 = hash(floor(cx) * 1.73 + seed + f32(l) * 19.0);
                let speed = 1.2 + 2.0 * hash(floor(cx) * 2.9 + f32(l));
                let yy = fract(h1 + tt * speed * 0.12) * 7.0 - 1.0;
                let xx = (fract(cx) - 0.5 + 0.2 * sin(tt * 3.0 + h1 * 6.0)) / sc;
                let de = length(vec2<f32>(xx, sy - yy));
                let lit = step(0.35, h1) * eenv * (1.0 - smoothstep(4.0, 6.0, yy));
                glow = glow + exp(-sq(de / 0.08)) * lit * 1.4;
                core = core + exp(-sq(de / 0.035)) * lit * 0.7;
            }
        }
    }
    return zfinish(col, glow, core, white, dark, f.color.a);
}

// ---- Rift Capsule opening (docs/fx/CAPSULE_OPENING.md) ----

// A pill along `up` through the origin: half length h, radius r. `half` keeps one end only
// (1 the top, -1 the bottom, 0 all of it), cut flat across the seam.
fn cap_sdf(p: vec3<f32>, up: vec3<f32>, h: f32, r: f32, half: f32) -> f32 {
    let y = dot(p, up);
    let d = length(p - up * clamp(y, -h, h)) - r;
    if (half == 0.0) { return d; }
    return max(d, -y * half);
}

// Where the view ray first meets the pill at c (t along the ray), or -1.
fn cap_hit(rd: vec3<f32>, tmax: f32, c: vec3<f32>, up: vec3<f32>, h: f32, r: f32, half: f32) -> f32 {
    let tc = dot(c, rd);
    if (length(rd * tc - c) > h + r || r < 0.002) { return -1.0; }
    var t = max(tc - h - r - 0.02, 0.0);
    let t_end = min(tc + h + r, tmax);
    for (var i = 0; i < 32; i = i + 1) {
        let d = cap_sdf(rd * t - c, up, h, r, half);
        if (d < 0.0015) { return select(-1.0, t, t < t_end); }
        t = t + d;
        if (t > t_end) { break; }
    }
    return -1.0;
}

// The pill's shell where the ray meets it: rarity colour on top, white below, a dark seam with a
// button facing the camera; `ch` (0-1) lights the seam, the button and cracks from inside.
fn cap_shade(p: vec3<f32>, rd: vec3<f32>, up: vec3<f32>, side: vec3<f32>, h: f32, r: f32, half: f32, col: vec3<f32>, ch: f32, seed: f32) -> vec3<f32> {
    let e = 0.002;
    let n = normalize(vec3<f32>(
        cap_sdf(p + vec3<f32>(e, 0.0, 0.0), up, h, r, half) - cap_sdf(p - vec3<f32>(e, 0.0, 0.0), up, h, r, half),
        cap_sdf(p + vec3<f32>(0.0, e, 0.0), up, h, r, half) - cap_sdf(p - vec3<f32>(0.0, e, 0.0), up, h, r, half),
        cap_sdf(p + vec3<f32>(0.0, 0.0, e), up, h, r, half) - cap_sdf(p - vec3<f32>(0.0, 0.0, e), up, h, r, half)));
    let y = dot(p, up) / r;
    let lum = dot(col, vec3<f32>(0.299, 0.587, 0.114));
    let deep = max(mix(vec3<f32>(lum), col, 1.3), vec3<f32>(0.0)) * 0.85;
    var base = mix(vec3<f32>(0.9, 0.9, 0.93), deep, smoothstep(-0.03, 0.03, y));
    let seam = 1.0 - smoothstep(0.05, 0.1, abs(y));
    base = mix(base, vec3<f32>(0.08, 0.07, 0.1), seam);
    let l = normalize(vec3<f32>(0.35, 0.85, -0.4));
    let diff = max(dot(n, l), 0.0) * 0.7 + 0.3;
    let spec = pow(max(dot(reflect(rd, n), l), 0.0), 36.0) * 0.7;
    let rim = pow(1.0 - max(dot(n, -rd), 0.0), 3.0);
    var c = base * diff + vec3<f32>(spec);
    // The seam glows as it charges, and so does the button on the front.
    let lit = col * (0.2 + 3.0 * ch * ch) + vec3<f32>(1.0) * ch * ch * 1.2;
    c = c + lit * seam * (0.4 + 0.6 * ch);
    let front = max(dot(n, -rd), 0.0);
    let button = (1.0 - smoothstep(0.2, 0.26, abs(y))) * smoothstep(0.93, 0.97, front);
    c = mix(c, vec3<f32>(0.95) + lit * 0.8, button);
    // Cracks of light, splitting from the seam as it nears the burst.
    let ang = atan2(dot(p, side), dot(p, cross(up, side)));
    let k = smoothstep(0.55, 1.0, ch);
    let crack = ln(abs(fract(ang * 1.1 + 0.25 * noise1(y * 6.0 + seed) + hash(seed)) - 0.5), 0.025) * (1.0 - smoothstep(k * 1.4, k * 1.4 + 0.1, abs(y)));
    c = c + (col * 2.0 + vec3<f32>(1.0)) * crack * k * 1.5;
    c = c + col * rim * (0.15 + 0.9 * ch);
    return c;
}

// A Rift Capsule opening (a: where it floats, r: its radius, p.w: the clock span so tt is seconds
// in, m.x: the burst time, m.y: the tier, 0 common to 2 legendary). It grows in, shakes and
// charges with motes and rings closing in, bursts with one flash, a fireball and a beam in its
// rarity's colour, its halves fly apart, a shock ring runs out, sparks fall, and the prize (a trail
// of light) circles where it was. The tier sets the beam's width and how bright it all is.
fn capfx(f: Fx, rd: vec3<f32>, tmax: f32, time: f32) -> vec4<f32> {
    let c0 = f.a.xyz;
    let s = max(f.a.w, 0.05);
    let tt = clamp(f.p.x, 0.0, 1.0) * f.p.w;
    let tb = f.m.x;
    let tier = clamp(f.m.y, 0.0, 2.0);
    let seed = f.p.y;
    let col = f.color.rgb;
    let k = 0.65 + 0.4 * tier;
    var glow = 0.0;
    var core = 0.0;
    var white = 0.0;
    var body = vec4<f32>(0.0);
    let vdir = normalize(c0);
    let side = normalize(cross(vec3<f32>(0.0, 1.0, 0.0), vdir) + vec3<f32>(1e-4, 0.0, 0.0));
    let ts = tt - tb;
    let ch = clamp((tt - 0.45) / max(tb - 0.45, 0.1), 0.0, 1.0);
    if (ts < 0.0) {
        // It grows in (with a little overshoot), then shakes harder and faster as it charges.
        let ap = clamp(tt / 0.45, 0.0, 1.0);
        let grow = 1.0 + 2.7 * pow(ap - 1.0, 3.0) + 1.7 * pow(ap - 1.0, 2.0);
        let amp = (0.05 + 0.3 * ch * ch) * step(0.45, tt);
        let tilt = sin(tt * (9.0 + 26.0 * ch)) * amp * (0.65 + 0.35 * sin(tt * 3.1 + seed));
        let up = normalize(vec3<f32>(0.0, cos(tilt), 0.0) + side * sin(tilt));
        let c = c0 + vec3<f32>(0.0, abs(sin(tt * (4.5 + 13.0 * ch))) * 0.05 * ch * ch, 0.0) + side * sin(tt * 31.0) * 0.012 * ch;
        let r = s * grow;
        let h = s * 0.55 * grow;
        let th = cap_hit(rd, tmax, c, up, h, r, 0.0);
        if (th > 0.0) {
            body = vec4<f32>(cap_shade(rd * th - c, rd, up, side, h, r, 0.0, col, ch, seed), 1.0);
        }
        // A halo that swells with the charge.
        let hb = ray_ball(rd, tmax, c, s * 6.0);
        glow = glow + exp(-hb.x / (s * (0.9 + 1.6 * ch))) * (0.12 + 0.5 * ch) * hb.y * ap;
        // Motes of light spiralling in from all round, faster as it charges.
        for (var i = 0; i < 12; i = i + 1) {
            let fi = f32(i);
            let ph = fract(tt * (0.45 + 0.6 * ch) + hash(fi * 3.7 + seed));
            let a1 = hash(fi * 1.9 + seed) * 6.2832 + ph * 3.0;
            let a2 = (hash(fi * 5.3 + seed) - 0.5) * 2.4;
            let dir = vec3<f32>(cos(a1) * cos(a2), sin(a2), sin(a1) * cos(a2));
            let pos = c + dir * mix(s * 9.0, s * 1.1, ph * ph);
            let tp = clamp(dot(pos, rd), 0.0, tmax);
            let dm = length(rd * tp - pos);
            let lit = smoothstep(0.0, 0.15, ph) * step(tp, tmax - 0.01) * step(0.2, tt);
            glow = glow + exp(-sq(dm / (s * 0.22))) * lit * (0.5 + 0.8 * ch);
            core = core + exp(-sq(dm / (s * 0.07))) * lit * 0.8;
        }
        // Rings closing in on it, faster near the end.
        for (var i = 0; i < 2; i = i + 1) {
            let ph = fract(tt * (0.8 + 1.6 * ch) + f32(i) * 0.5);
            let rr = s * (5.5 - 4.3 * ph);
            glow = glow + ln(hb.x - rr, s * 0.08) * ph * ch * 0.9 * hb.y;
        }
        // The last moment: it pulls the light in tight.
        let pull = smoothstep(0.85, 1.0, ch);
        core = core + exp(-sq(hb.x / (s * 1.2))) * pull * 1.2 * hb.y;
    } else {
        // One flash over the whole picture as it bursts (a single pulse, never a strobe).
        let flash = exp(-sq((ts - 0.04) / 0.07));
        white = white + flash * (0.3 + 0.12 * tier);
        glow = glow + flash * 0.25;
        // A fireball swelling out of it (kept to a few times the capsule: it floats close to the eye).
        let fb = clamp(ts / 0.3, 0.0, 1.0);
        let rr = s * (1.0 + 3.0 * (1.0 - pow(1.0 - fb, 3.0)));
        let hb = ray_ball(rd, tmax, c0, max(rr, s));
        let fenv = 1.0 - smoothstep(0.04, 0.4, ts);
        glow = glow + (ln(hb.x - rr, rr * 0.12) * 1.2 + step(hb.x, rr) * 0.2) * fenv * hb.y;
        core = core + exp(-sq(hb.x / (rr * 0.45))) * fenv * 1.5 * hb.y;
        // The two halves fly apart, tumbling, and are gone in half a second.
        let gone = clamp(ts / 0.5, 0.0, 1.0);
        if (gone < 1.0) {
            for (var i = 0; i < 2; i = i + 1) {
                let sg = 1.0 - 2.0 * f32(i);
                let spin = sg * ts * 9.0;
                let up = normalize(vec3<f32>(0.0, cos(spin), 0.0) + side * sin(spin));
                let off = vec3<f32>(0.0, sg * (1.8 * ts) - 3.0 * ts * ts, 0.0) + side * sg * 1.3 * ts;
                let r = s * (1.0 - gone * 0.6);
                let th = cap_hit(rd, tmax, c0 + off, up, s * 0.55, r, sg);
                if (th > 0.0 && body.a < 0.5) {
                    let a = 1.0 - gone;
                    body = vec4<f32>(cap_shade(rd * th - c0 - off, rd, up, side, s * 0.55, r, sg, col, 1.0 - gone, seed) * a, a);
                }
            }
        }
        // The beam: straight up from where it was, in its rarity's colour, wider the rarer.
        let benv = smoothstep(0.0, 0.08, ts) * (1.0 - smoothstep(2.2, 3.4, ts));
        if (benv > 0.0) {
            let bot = c0 - vec3<f32>(0.0, s * 1.5, 0.0);
            let top = c0 + vec3<f32>(0.0, 70.0, 0.0);
            let q = ray_seg(rd, tmax, bot, top);
            let axis = bot + (top - bot) * q.w;
            let d = length(q.xyz - axis);
            let w = s * (0.45 + 0.4 * tier) * (1.0 + 1.4 * exp(-ts * 5.0));
            let x = axis.y - c0.y;
            let foot = smoothstep(-s * 1.5, 0.0, x);
            let bands = 0.75 + 0.25 * sin(x * 2.2 - ts * 16.0);
            core = core + exp(-sq(d / (w * 0.32))) * benv * foot * 1.4 * bands;
            glow = glow + (exp(-sq(d / (w * 1.1))) * 0.5 + exp(-d / (w * 2.0)) * 0.35) * benv * foot;
            white = white + exp(-sq(d / (w * 0.14))) * benv * foot * (0.35 + 0.35 * tier);
            // Legendary: two strands winding up round it.
            if (tier > 1.5 && d < w * 6.0) {
                let up = normalize(top - bot);
                let u1 = perp(up);
                let u2 = cross(up, u1);
                let phi = x * 1.6 - ts * 7.0 + seed;
                let s1 = axis + (u1 * cos(phi) + u2 * sin(phi)) * w * 1.6;
                let s2 = axis - (u1 * cos(phi) + u2 * sin(phi)) * w * 1.6;
                let ds = min(length(q.xyz - s1), length(q.xyz - s2));
                glow = glow + exp(-sq(ds / (w * 0.25))) * benv * foot * 1.2;
            }
        }
        // A shock ring racing out level with it.
        if (abs(rd.y) > 1e-3) {
            let tp = c0.y / rd.y;
            if (tp > 0.0 && tp < tmax) {
                let hd = length((rd * tp - c0).xz);
                let rk = clamp(ts / 0.9, 0.0, 1.0);
                let ring = s * (2.0 + 26.0 * (1.0 - pow(1.0 - rk, 2.0)));
                let rw = s * (0.25 + 1.2 * rk);
                glow = glow + ln(hd - ring, rw) * sq(1.0 - rk) * (0.8 + 0.4 * tier);
                white = white + ln(hd - ring, rw * 0.3) * sq(1.0 - rk) * 0.4;
            }
        }
        // Sparks falling where it burst, more and longer the rarer.
        let senv = smoothstep(0.05, 0.3, ts) * (1.0 - smoothstep(2.6 + tier, 3.6 + tier, ts));
        let tcb = dot(c0, rd);
        if (senv > 0.0 && tcb > 0.0 && tcb < tmax + 1.0) {
            let v = rd * tcb - c0;
            let sx = dot(v, side) / s;
            let sy = v.y / s;
            if (abs(sx) < 14.0 && sy > -16.0 && sy < 10.0) {
                for (var l = 0; l < 2; l = l + 1) {
                    let sc = 0.55 + f32(l) * 0.35;
                    let cx = sx * sc + f32(l) * 0.5;
                    let h1 = hash(floor(cx) * 1.73 + seed + f32(l) * 19.0);
                    let fall = 1.5 + 2.5 * hash(floor(cx) * 2.9 + f32(l));
                    let yy = 8.0 * (1.0 - h1) - ts * fall * (1.0 + ts * 0.4);
                    let xx = (fract(cx) - 0.5 + 0.25 * sin(ts * 4.0 + h1 * 6.0)) / sc;
                    let de = length(vec2<f32>(xx, sy - yy));
                    let on = step(0.45 - 0.15 * tier, h1) * senv;
                    let tw = 0.6 + 0.4 * sin(time * 20.0 + h1 * 40.0);
                    glow = glow + exp(-sq(de / 0.45)) * on * 1.1 * tw;
                    white = white + exp(-sq(de / 0.16)) * on * 0.6 * tw;
                }
            }
        }
        // The prize: a ribbon of light looping where the capsule was, with a fading tail.
        let penv = smoothstep(0.25, 0.7, ts) * (1.0 - smoothstep(4.0, 4.4, ts));
        if (penv > 0.0) {
            let hp = ray_ball(rd, tmax, c0, s * 4.0);
            if (hp.y > 0.0 && hp.x < s * 4.0) {
                let u2 = normalize(cross(side, vdir));
                for (var j = 0; j < 10; j = j + 1) {
                    let lag = f32(j) * 0.07;
                    let a = (ts - lag) * 4.2 + seed;
                    let pos = c0 + (side * cos(a) * 2.2 + u2 * sin(a * 2.0) * 0.9 + vdir * sin(a) * 1.4) * s;
                    let tp = clamp(dot(pos, rd), 0.0, tmax);
                    let dm = length(rd * tp - pos);
                    let fade = 1.0 - f32(j) / 10.0;
                    glow = glow + exp(-sq(dm / (s * 0.3))) * fade * penv * 0.9;
                    core = core + exp(-sq(dm / (s * 0.1))) * fade * fade * penv * 1.2;
                }
            }
        }
    }
    let lum = dot(col, vec3<f32>(0.299, 0.587, 0.114));
    let cs = max(mix(vec3<f32>(lum), col, 1.4), vec3<f32>(0.0));
    var light = cs * glow * 1.1 * k + mix(cs, vec3<f32>(1.0), 0.35) * core * 1.6 * k;
    let peak = max(light.r, max(light.g, light.b));
    light = light * min(1.0, 2.2 / max(peak, 1e-4)) + vec3<f32>(1.0) * min(white, 2.5);
    return vec4<f32>(body.rgb + light * f.color.a, body.a);
}

// ---- The trainer kit (fx_trainer.rs): Poké Ball, Tether Rope, aim line ----

// `v` turned `ang` radians about the unit axis `k`.
fn rot_about(v: vec3<f32>, k: vec3<f32>, ang: f32) -> vec3<f32> {
    let c = cos(ang);
    let s = sin(ang);
    return v * c + cross(k, v) * s + k * dot(k, v) * (1.0 - c);
}

// Half a ball about the origin: radius r, pole `up`; `half` 1 keeps the pole's side, -1 the other.
fn hball_sdf(p: vec3<f32>, up: vec3<f32>, r: f32, half: f32) -> f32 {
    return max(length(p) - r, -dot(p, up) * half);
}

// Where the view ray first meets the half ball at c (t along the ray), or -1.
fn hball_hit(rd: vec3<f32>, tmax: f32, c: vec3<f32>, up: vec3<f32>, r: f32, half: f32) -> f32 {
    let tc = dot(c, rd);
    if (tc < -r || length(rd * tc - c) > r || r < 0.002) { return -1.0; }
    var t = max(tc - r - 0.01, 0.0);
    let t_end = min(tc + r, tmax);
    for (var i = 0; i < 24; i = i + 1) {
        let d = hball_sdf(rd * t - c, up, r, half);
        if (d < 0.0012) { return select(-1.0, t, t < t_end); }
        t = t + d;
        if (t > t_end) { break; }
    }
    return -1.0;
}

// The half ball's surface at p (from its centre): `col` on the pole's side or white on the other,
// the black band and the button (toward `fwd`) on the seam, and the flat face inside, lit by `lit`.
fn hball_shade(p: vec3<f32>, rd: vec3<f32>, up: vec3<f32>, fwd: vec3<f32>, r: f32, half: f32, col: vec3<f32>, lit: f32) -> vec3<f32> {
    let y = dot(p, up) / r;
    let rad = length(p) / r;
    let sun = normalize(vec3<f32>(0.35, 0.85, -0.4));
    if (abs(y) < 0.03 && rad < 0.97) {
        // The cut face: a dark inside with a pale rim, glowing while it pulls its target in.
        let n = -up * half;
        let rim = smoothstep(0.78, 0.9, rad);
        let inside = mix(vec3<f32>(0.1, 0.1, 0.13), vec3<f32>(0.75, 0.75, 0.78), rim);
        let d = max(dot(n, sun), 0.0) * 0.5 + 0.5;
        return inside * d + (vec3<f32>(1.0, 0.92, 0.9) * 1.6 + col * 0.8) * lit * (1.0 - rim * 0.6);
    }
    let n = normalize(p);
    var base = vec3<f32>(0.94, 0.94, 0.96);
    if (half > 0.0) { base = col; }
    let band = 1.0 - smoothstep(0.075, 0.105, abs(y));
    base = mix(base, vec3<f32>(0.05, 0.05, 0.07), band);
    // The button: a black ring with a white middle, where the seam faces `fwd`.
    let bd = length(n - fwd);
    base = mix(base, vec3<f32>(0.05, 0.05, 0.07), 1.0 - smoothstep(0.3, 0.34, bd));
    let mid = 1.0 - smoothstep(0.18, 0.22, bd);
    base = mix(base, vec3<f32>(0.96, 0.96, 0.98), mid);
    let diff = max(dot(n, sun), 0.0) * 0.6 + 0.4;
    let spec = pow(max(dot(reflect(rd, n), sun), 0.0), 40.0) * 0.55;
    let rim = pow(1.0 - max(dot(n, -rd), 0.0), 3.0) * 0.18;
    return base * diff + vec3<f32>(spec + rim) + vec3<f32>(1.0, 0.25, 0.2) * mid * lit * 1.6;
}

// A Poké Ball at a (radius a.w). m: x how far the lid is open (0-1), y its tilt (radians, rocking
// side to side), z the button's glow, w its spin (flying) or its size (caught). Flags: 0 flying
// (b: where it just was), 1 opening and pulling its target at b in (color.a: the pull), 2 hanging,
// 3 caught (stars), 4 burst open, 5 a miss fading (color.a).
// The lens round a bright point `c` (after Orbital Laser's lens stage): x a soft bloom, y a thin
// level streak through it. `size` is the bloom's reach in blocks; nothing past 4.5 of them.
fn lens(c: vec3<f32>, rd: vec3<f32>, tmax: f32, size: f32) -> vec2<f32> {
    let tc = dot(c, rd);
    if (tc <= 0.0 || tc > tmax + 0.6) { return vec2<f32>(0.0); }
    let w = rd * tc - c;
    let sd = normalize(cross(rd, vec3<f32>(0.0, 1.0, 0.0)) + vec3<f32>(1e-5, 0.0, 0.0));
    let x = dot(w, sd);
    let y = dot(w, cross(sd, rd));
    let win = 1.0 - smoothstep(size * 2.0, size * 4.5, length(w));
    let bloom = win / (1.0 + dot(w, w) / (size * size * 0.09));
    let streak = exp(-abs(y) / (size * 0.035)) * exp(-abs(x) / (size * 1.3)) * win;
    return vec2<f32>(bloom, streak);
}

fn ballfx(f: Fx, rd: vec3<f32>, tmax: f32, time: f32) -> vec4<f32> {
    let c = f.a.xyz;
    var r = f.a.w;
    let age = f.p.x;
    let seed = f.p.y;
    let ph = i32(floor(f.p.z / 32.0 + 0.001));
    let col = f.color.rgb;
    let k = f.color.a;
    // Nothing of it reaches further than this from its centre (the pull's beam aside).
    let reach = length(f.b.xyz - c) * 0.5 + r * 9.0 + 1.5;
    let bc = (c + f.b.xyz) * 0.5;
    let btc = clamp(dot(bc, rd), 0.0, tmax);
    if (length(rd * btc - bc) > reach) { return vec4<f32>(0.0); }
    // Its own frame: the button faces the camera, level; the tilt rocks it about that line.
    var fwd = normalize(vec3<f32>(-c.x, 0.0, -c.z) + vec3<f32>(1e-4, 0.0, 0.0));
    let side = normalize(cross(vec3<f32>(0.0, 1.0, 0.0), fwd));
    var up = vec3<f32>(0.0, 1.0, 0.0);
    if (ph == 0 || ph == 5) {
        up = rot_about(up, side, f.m.w);
        fwd = rot_about(fwd, side, f.m.w);
    }
    up = rot_about(up, fwd, f.m.y);
    var body = vec4<f32>(0.0);
    var glow = 0.0;
    var white = 0.0;
    var light = vec3<f32>(0.0);
    if (ph == 3) { r = r * max(f.m.w, 0.0); }
    if (ph == 4) {
        // Burst open: the halves fly apart tumbling, a white flash and a ring of light.
        for (var i = 0; i < 2; i = i + 1) {
            let sg = 1.0 - 2.0 * f32(i);
            let spin = sg * age * 7.0;
            let hup = rot_about(up, fwd, spin);
            let off = vec3<f32>(0.0, sg * 1.1 * age - 1.6 * age * age + 0.5 * age, 0.0) + side * sg * 1.2 * age;
            let rr = r * (1.0 - age * 0.5);
            let th = hball_hit(rd, tmax, c + off, hup, rr, sg);
            if (th > 0.0 && body.a < 0.5) {
                let a = 1.0 - smoothstep(0.55, 1.0, age);
                body = vec4<f32>(hball_shade(rd * th - c - off, rd, hup, fwd, rr, sg, col, 1.0 - age) * a, a);
            }
        }
        let hb = ray_ball(rd, tmax, c, r * 4.0);
        let fenv = 1.0 - smoothstep(0.0, 0.5, age);
        let rr = r * (1.0 + 7.0 * (1.0 - pow(1.0 - age, 3.0)));
        white = exp(-sq(hb.x / (r * 2.2))) * fenv * 1.4 * hb.y;
        glow = ln(hb.x - rr, r * 0.5) * (1.0 - age) * hb.y;
        light = vec3<f32>(1.0, 0.95, 0.9) * (white + glow);
        // The lens on the flash, and sparks thrown out of the break, falling.
        let ln2 = lens(c, rd, tmax, 0.8);
        light = light + vec3<f32>(1.0, 0.9, 0.8) * ln2.x * fenv * 0.9 + vec3<f32>(1.0, 0.55, 0.4) * ln2.y * fenv * 1.3;
        var sparks = 0.0;
        for (var i = 0; i < 12; i = i + 1) {
            let fi = f32(i);
            let az = hash(fi * 7.3 + seed) * 6.2832;
            let el = hash(fi * 3.1 + seed * 1.7) * 1.2 - 0.2;
            let sp = 1.6 + 1.8 * hash(fi * 5.7 + seed);
            let dir = vec3<f32>(cos(az) * cos(el), sin(el), sin(az) * cos(el));
            let pp = c + dir * sp * age + vec3<f32>(0.0, -2.2 * age * age, 0.0);
            let tp = dot(pp, rd);
            if (tp > 0.0 && tp < tmax) {
                sparks = sparks + exp(-sq(length(rd * tp - pp) / 0.035)) * (0.6 + 0.4 * sin(time * 40.0 + fi * 2.0));
            }
        }
        light = light + vec3<f32>(1.0, 0.75, 0.4) * sparks * (1.0 - age) * smoothstep(0.0, 0.06, age) * 2.4;
    } else {
        // The lid swings up on a hinge at the back of the seam.
        let ang = f.m.x * 1.25;
        let hk = normalize(cross(fwd, up));
        let hinge = c - fwd * r;
        let ctop = hinge + rot_about(fwd * r, hk, ang);
        let utop = rot_about(up, hk, ang);
        let ftop = rot_about(fwd, hk, ang);
        let tb = hball_hit(rd, tmax, c, up, r, -1.0);
        let tt = hball_hit(rd, tmax, ctop, utop, r, 1.0);
        var a = 1.0;
        if (ph == 5) { a = clamp(k, 0.0, 1.0); }
        if (tb > 0.0 && (tt < 0.0 || tb <= tt)) {
            body = vec4<f32>(hball_shade(rd * tb - c, rd, up, fwd, r, -1.0, col, f.m.z * f.m.x) * a, a);
        } else if (tt > 0.0) {
            body = vec4<f32>(hball_shade(rd * tt - ctop, rd, utop, ftop, r, 1.0, col, f.m.z) * a, a);
        }
        let hb = ray_ball(rd, tmax, c, r * 3.0);
        if (ph == 0) {
            // A pale streak behind it as it flies.
            let q = ray_seg(rd, tmax, f.b.xyz, c);
            let d = length(q.xyz - (f.b.xyz + (c - f.b.xyz) * q.w));
            glow = exp(-sq(d / (r * 0.55))) * q.w * 0.35 * k;
            light = vec3<f32>(1.0) * glow;
        } else if (ph == 1) {
            // The pull: a beam of light from the target into the open ball, motes streaming up it,
            // and the ball's mouth glowing.
            let tg = f.b.xyz;
            let q = ray_seg(rd, tmax, tg, c);
            let axis = tg + (c - tg) * q.w;
            let d = length(q.xyz - axis);
            let w = r * (0.5 + 1.6 * (1.0 - q.w));
            let beam = (exp(-sq(d / w)) * 0.45 + exp(-sq(d / (w * 0.3))) * 0.5) * k;
            var motes = 0.0;
            for (var i = 0; i < 8; i = i + 1) {
                let fi = f32(i);
                let s = fract(time * 2.6 + hash(fi * 3.3 + seed));
                let aa = hash(fi * 5.1) * 6.2832 + s * 5.0;
                let sw = (1.0 - s) * 0.55;
                let pp = mix(tg, c, s) + (side * cos(aa) + vec3<f32>(0.0, 1.0, 0.0) * sin(aa)) * sw;
                let tp = clamp(dot(pp, rd), 0.0, tmax);
                motes = motes + exp(-sq(length(rd * tp - pp) / 0.07)) * step(tp, tmax - 0.01);
            }
            // The target itself, as a ball of light shrinking into the beam.
            let tb2 = ray_ball(rd, tmax, tg, 0.9);
            let shell = exp(-sq(tb2.x / (0.75 * (1.0 - age * 0.8)))) * k * tb2.y;
            white = exp(-sq(hb.x / (r * 1.1))) * f.m.x * 0.5 * hb.y + motes * k * 0.9 + beam * 0.25;
            light = vec3<f32>(1.0, 0.3, 0.25) * (beam + shell * 0.7) + vec3<f32>(1.0) * white;
            // The lens on the open mouth.
            let ln2 = lens(c, rd, tmax, 0.7);
            light = light + vec3<f32>(1.0, 0.6, 0.55) * ln2.x * f.m.x * 0.5 + vec3<f32>(1.0, 0.45, 0.4) * ln2.y * f.m.x * 0.8;
        } else if (ph == 2) {
            // Each rock: a red pulse from the button.
            glow = exp(-hb.x / (r * 0.9)) * f.m.z * 0.5 * hb.y;
            light = vec3<f32>(1.0, 0.2, 0.15) * glow;
        } else if (ph == 3) {
            // Caught: a white click, a gold ring, and three stars thrown up and out.
            let click = 1.0 - smoothstep(0.0, 0.25, age);
            white = exp(-sq(hb.x / (f.a.w * 1.6))) * click * hb.y;
            let rr = f.a.w * (1.0 + 5.0 * age);
            glow = ln(hb.x - rr, f.a.w * 0.35) * (1.0 - smoothstep(0.2, 0.6, age)) * hb.y;
            var stars = 0.0;
            let senv = smoothstep(0.02, 0.12, age) * (1.0 - smoothstep(0.6, 0.95, age));
            for (var i = 0; i < 3; i = i + 1) {
                let fi = f32(i) - 1.0;
                let pp = c + side * fi * (0.25 + 1.1 * age) + vec3<f32>(0.0, 0.25 + 1.9 * age - 1.7 * age * age - abs(fi) * 0.15, 0.0);
                let tp = dot(pp, rd);
                if (tp > 0.0 && tp < tmax) {
                    let w2 = rd * tp - pp;
                    let sp = rot2(vec2<f32>(dot(w2, side), w2.y), age * 5.0 + fi);
                    // A four-point star: bright along either axis, pinched to the centre.
                    let st = exp(-(abs(sp.x) * abs(sp.y)) / 0.0009) * exp(-dot(sp, sp) / 0.022);
                    stars = stars + st;
                }
            }
            light = vec3<f32>(1.0) * white + vec3<f32>(1.0, 0.82, 0.25) * (glow * 0.9 + stars * senv * 2.2);
            // The lens on the click.
            let ln2 = lens(c, rd, tmax, 0.7);
            light = light + vec3<f32>(1.0, 0.95, 0.8) * ln2.x * click * 0.8 + vec3<f32>(1.0, 0.85, 0.4) * ln2.y * click * 1.3;
        }
    }
    return vec4<f32>(body.rgb + light * (1.0 - body.a * 0.6), body.a);
}

// The Tether Rope from a (the hand) to b (the target's middle): radius a.w, m.x how far it sags,
// m.y the loop's radius round the target (0: not there yet), m.z the strain (0-1, it reddens and
// shivers before it snaps). Two strands twisted, solid.
fn ropefx(f: Fx, rd: vec3<f32>, tmax: f32, time: f32) -> vec4<f32> {
    let a = f.a.xyz;
    let b = f.b.xyz;
    let r = f.a.w;
    let sag = f.m.x;
    let lr = f.m.y;
    let strain = f.m.z;
    let bc = (a + b) * 0.5;
    let br = length(b - a) * 0.5 + sag + lr + 0.3;
    let btc = clamp(dot(bc, rd), 0.0, tmax);
    if (length(rd * btc - bc) > br) { return vec4<f32>(0.0); }
    let len = max(length(b - a), 0.1);
    let shiver = sin(time * 31.0) * 0.02 * strain;
    var best = 1e9;
    var bs = 0.0;
    var bp = vec3<f32>(0.0);
    var bax = vec3<f32>(0.0);
    var bdir = vec3<f32>(1.0, 0.0, 0.0);
    var prev = a;
    for (var i = 1; i <= 10; i = i + 1) {
        let s = f32(i) / 10.0;
        let cur = mix(a, b, s) - vec3<f32>(0.0, (sag + shiver) * 4.0 * s * (1.0 - s), 0.0);
        let q = ray_seg(rd, tmax, prev, cur);
        let ax = prev + (cur - prev) * q.w;
        let d = length(q.xyz - ax);
        if (d < best) {
            best = d;
            bs = (f32(i) - 1.0 + q.w) / 10.0 * len;
            bp = q.xyz;
            bax = ax;
            bdir = normalize(cur - prev);
        }
        prev = cur;
    }
    if (lr > 0.0) {
        // The loop round the target: a level ring, drawn as twelve short pieces.
        var pl = b + vec3<f32>(lr, 0.0, 0.0);
        for (var j = 1; j <= 12; j = j + 1) {
            let aa = f32(j) / 12.0 * 6.2832;
            let cur = b + vec3<f32>(cos(aa) * lr, sin(aa * 2.0 + time * 2.0) * 0.03, sin(aa) * lr);
            let q = ray_seg(rd, tmax, pl, cur);
            let ax = pl + (cur - pl) * q.w;
            let d = length(q.xyz - ax);
            if (d < best) {
                best = d;
                bs = len + (f32(j) - 1.0 + q.w) / 12.0 * 6.2832 * lr;
                bp = q.xyz;
                bax = ax;
                bdir = normalize(cur - pl);
            }
            pl = cur;
        }
    }
    let seen = length(bp) < tmax - 0.02;
    if (best < r && seen) {
        let lat = (bp - bax) / max(best, 1e-4);
        let e = clamp(best / r, 0.0, 1.0);
        let n = normalize(lat * e - rd * sqrt(1.0 - e * e));
        // Two strands winding round each other: bands slanting across it.
        let across = dot(bp - bax, normalize(cross(bdir, rd) + vec3<f32>(1e-4, 0.0, 0.0))) / r;
        let tw = 0.5 + 0.5 * sin(bs * 34.0 + across * 2.4);
        var col = mix(f.color.rgb * 0.55, f.color.rgb * 1.15, smoothstep(0.2, 0.8, tw));
        col = mix(col, vec3<f32>(0.95, 0.25, 0.15), strain * (0.22 + 0.2 * sin(time * 16.0)));
        let sun = normalize(vec3<f32>(0.35, 0.85, -0.4));
        let diff = max(dot(n, sun), 0.0) * 0.6 + 0.4;
        return vec4<f32>(col * diff, 1.0);
    }
    // A thin dark edge, so it reads against grass and sky alike.
    if (best < r * 1.5 && seen) { return vec4<f32>(0.0, 0.0, 0.0, 0.45); }
    return vec4<f32>(0.0);
}

// The faint line of a throw being aimed, from the hand a to where it will land b: dashes running
// along an arc m.x high, and a ring on the ground at b (gold when a Pokémon is under it).
fn arcfx(f: Fx, rd: vec3<f32>, tmax: f32, time: f32) -> vec4<f32> {
    let a = f.a.xyz;
    let b = f.b.xyz;
    let h = f.m.x;
    let bc = (a + b) * 0.5 + vec3<f32>(0.0, h * 0.5, 0.0);
    let br = length(b - a) * 0.5 + h + 1.0;
    let btc = clamp(dot(bc, rd), 0.0, tmax);
    if (length(rd * btc - bc) > br) { return vec4<f32>(0.0); }
    let len = max(length(b - a), 0.1);
    var best = 1e9;
    var bs = 0.0;
    var bp = vec3<f32>(0.0);
    var prev = a;
    for (var i = 1; i <= 12; i = i + 1) {
        let s = f32(i) / 12.0;
        let cur = mix(a, b, s) + vec3<f32>(0.0, h * 4.0 * s * (1.0 - s), 0.0);
        let q = ray_seg(rd, tmax, prev, cur);
        let d = length(q.xyz - (prev + (cur - prev) * q.w));
        if (d < best) {
            best = d;
            bs = (f32(i) - 1.0 + q.w) / 12.0;
            bp = q.xyz;
        }
        prev = cur;
    }
    var g = 0.0;
    if (length(bp) < tmax - 0.02) {
        let run = fract(bs * len * 1.6 - time * 1.5);
        let dash = smoothstep(0.35, 0.5, run) * (1.0 - smoothstep(0.85, 1.0, run));
        // Nothing right at the hand (it would cross the trainer's own view), fading in.
        g = exp(-sq(best / max(f.a.w, length(bp) * 0.0045))) * dash * smoothstep(0.06, 0.2, bs);
    }
    // The landing ring, on whatever the scene has there.
    if (tmax < 9000.0) {
        let p = rd * tmax - b;
        if (abs(p.y) < 0.9) {
            let dd = length(p.xz);
            g = g + ln(dd - 0.5, 0.07) * (1.6 + 0.4 * sin(time * 5.0)) + exp(-sq(dd / 0.1)) * 1.2;
        }
    }
    return vec4<f32>(f.color.rgb * g * f.color.a, 0.0);
}

fn seg2(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>) -> f32 {
    let pa = p - a;
    let ba = b - a;
    return length(pa - ba * clamp(dot(pa, ba) / dot(ba, ba), 0.0, 1.0));
}

// A trainer's order, over the Pokémon it names (a), facing the camera: m.x 0 a bullseye (Attack),
// 1 a gold Z (a Z-Move is wanted), 2 both (All-out). 3 is a spot on the ground (Move): a ring.
// Seen through walls, fainter, so the party can find its target.
fn markfx(f: Fx, rd: vec3<f32>, tmax: f32, time: f32) -> vec4<f32> {
    let kind = i32(f.m.x + 0.5);
    let fade = f.color.a;
    if (kind == 3) {
        if (tmax > 9000.0) { return vec4<f32>(0.0); }
        let p = rd * tmax - f.a.xyz;
        if (abs(p.y) > 0.9) { return vec4<f32>(0.0); }
        let dd = length(p.xz);
        let run = fract(time * 0.8);
        let g = ln(dd - 0.9, 0.07) * 1.5 + ln(dd - 0.9 * run, 0.05) * (1.0 - run) * 1.2 + exp(-sq(dd / 0.12)) * 1.4;
        return vec4<f32>(f.color.rgb * g * fade, 0.0);
    }
    let t = dot(f.a.xyz, rd);
    if (t <= 0.2) { return vec4<f32>(0.0); }
    // Never smaller on screen than a far-off one needs to be read.
    let r = max(f.a.w, t * 0.018) * (1.0 + 0.06 * sin(time * 5.0));
    let w = rd * t - f.a.xyz;
    if (length(w) > r * 2.6) { return vec4<f32>(0.0); }
    let side = normalize(cross(rd, vec3<f32>(0.0, 1.0, 0.0)));
    let upv = cross(side, rd);
    let uv = vec2<f32>(dot(w, side), dot(w, upv)) / r;
    // One pixel, in the mark's own units, for soft edges.
    let px = max(t * 0.0022 / r, 0.012);
    let ink = vec3<f32>(0.09, 0.09, 0.14);
    var col = vec3<f32>(0.0);
    var a = 0.0;
    if (kind == 0 || kind == 2) {
        let d = length(uv);
        let red = vec3<f32>(0.92, 0.18, 0.14);
        var c = red;
        c = mix(c, vec3<f32>(1.0), 1.0 - smoothstep(0.62 - px, 0.62 + px, d));
        c = mix(c, red, 1.0 - smoothstep(0.34 - px, 0.34 + px, d));
        c = mix(c, vec3<f32>(1.0), 1.0 - smoothstep(0.1 - px, 0.1 + px, d));
        c = mix(c, ink, smoothstep(0.88 - px, 0.88 + px, d));
        col = c;
        a = 1.0 - smoothstep(1.0 - px, 1.0 + px, d);
    }
    if (kind == 1 || kind == 2) {
        // Alone: in the middle. With the bullseye: smaller, on its upper right.
        var q = uv;
        if (kind == 2) { q = (uv - vec2<f32>(1.05, 0.8)) / 0.72; }
        let d = abs(q.x) + abs(q.y);
        let z = min(min(seg2(q, vec2<f32>(-0.3, 0.3), vec2<f32>(0.3, 0.3)), seg2(q, vec2<f32>(0.3, 0.3), vec2<f32>(-0.3, -0.3))), seg2(q, vec2<f32>(-0.3, -0.3), vec2<f32>(0.3, -0.3)));
        var c = vec3<f32>(1.0, 0.8, 0.22) * (0.9 + 0.25 * sin(time * 7.0));
        c = mix(c, ink, 1.0 - smoothstep(0.1 - px, 0.1 + px, z));
        c = mix(c, ink, smoothstep(0.86 - px, 0.86 + px, d));
        let za = 1.0 - smoothstep(1.0 - px, 1.0 + px, d);
        col = mix(col, c, za);
        a = max(a, za);
    }
    a = a * fade;
    if (t > tmax) { a = a * 0.4; }
    return vec4<f32>(col * a, a);
}

// Each type has its own motion and material (move effects spec, "Type signatures"):
// 0 normal, 1 fire, 2 water, 3 electric, 4 grass, 5 ice, 6 fighting, 7 poison, 8 ground, 9 flying,
// 10 psychic, 11 bug, 12 rock, 13 ghost, 14 dragon, 15 dark, 16 steel, 17 fairy. Flags (bits):
// bite 1, punch 2, slicing 4, pulse 8, bullet 16, wind 32, sound 64. Returns light (rgb) and how
// much to dim the scene behind (a).
// ---- Legendary shrines (server/shrine_fx.rs, docs/fx/SHRINE_FX.md) ----
// Layering, strands, sparks in beam space, the faint-through-walls floor and the Lorentzian halo
// follow Orbital Laser's beams (docs/fx/ORBITAL_LASER_REFERENCE.md rows 3-6); written again here.
// a: the shrine point (feet on its floor), radius: the capture ring. b: where the meteor is now
// (descent) or the column's top. flags: 0 descent, 1 awake, 2 being captured, 3 contested, 4 the
// crown. m: (capture 0-1, s since it woke, s in this state, descent s). The crown's clock is age * secs.

fn sh_gold() -> vec3<f32> { return vec3<f32>(1.0, 0.74, 0.2); }
fn sh_red() -> vec3<f32> { return vec3<f32>(1.0, 0.1, 0.06); }

// A type's meteor: (rocky body, glowing cracks, smoke in its wake, crackling arcs).
fn sh_kind(ty: i32) -> vec4<f32> {
    // fire: a burning rock
    if (ty == 1) { return vec4<f32>(0.85, 1.0, 1.0, 0.0); }
    // ground, rock: a boulder
    if (ty == 8 || ty == 12) { return vec4<f32>(1.0, 0.65, 0.8, 0.0); }
    // steel: an iron mass
    if (ty == 16) { return vec4<f32>(0.95, 0.45, 0.25, 0.0); }
    // dragon, dark: a comet in a scaled shell
    if (ty == 14 || ty == 15) { return vec4<f32>(0.75, 0.95, 0.55, 0.0); }
    // fighting, poison, grass, bug: a husk
    if (ty == 6 || ty == 7 || ty == 4 || ty == 11) { return vec4<f32>(0.6, 0.7, 0.35, 0.0); }
    // ghost: a dark heart in a wisp
    if (ty == 13) { return vec4<f32>(0.55, 0.0, 0.6, 0.0); }
    // electric: a plasma ball
    if (ty == 3) { return vec4<f32>(0.0, 0.0, 0.0, 1.0); }
    // water, ice, psychic, fairy, flying, normal: an orb of light
    return vec4<f32>(0.0);
}

// Cheap sparks in a strip's own space (Orbital row 5): x across, y along (already scrolled).
fn sh_sparks(x: f32, y: f32, dens: f32, seed: f32) -> f32 {
    let c = floor(vec2<f32>(x, y));
    let fr = fract(vec2<f32>(x, y)) - vec2<f32>(0.5);
    let h = hash(c.x * 7.13 + c.y * 3.71 + seed);
    let jit = vec2<f32>(hash(h * 91.0) - 0.5, hash(h * 37.0) - 0.5) * 0.5;
    return step(1.0 - dens, h) * exp(-sq(length(fr - jit) / 0.14));
}

// ---- Shrine arrivals: every type but fire has its own sequence (docs/fx/SHRINE_FX.md "Arrivals") ----
// Beats shared by all: a build-up seen from across the map (the descent's `fall` seconds), the climax
// at `fall` (the Z release burst, booms, the cry, one flash) with a type aftermath (`sh_climax`), then
// the column. P.foc is the sequence's focus (fx.rs `arrival_focus`): the dragon, the eclipse, the ball.
// Lightning (`sh_bolt`) follows Orbital Laser's bolt idea (a jagged segment, core plus glow; reference
// row 16); written again here.

struct ShP {
    base: vec3<f32>,
    foc: vec3<f32>,
    col: vec3<f32>,
    hot: vec3<f32>,
    gp: vec3<f32>,
    wt: f32,
    k: f32,
    fall: f32,
    time: f32,
    seed: f32,
    band: f32,
    hd: f32,
    gang: f32,
    camd: f32,
};
struct Sh { l: vec3<f32>, w: f32, d: f32 };

fn sh0() -> Sh { return Sh(vec3<f32>(0.0), 0.0, 0.0); }
fn sh_mix(a: Sh, b: Sh) -> Sh { return Sh(a.l + b.l, a.w + b.w, max(a.d, b.d)); }
// Thin things widen with distance so they still read from across the map.
fn sh_px(p: vec3<f32>) -> f32 { return 1.0 + length(p) / 70.0; }
// Smooth 2D value noise (domain-warped 1D noise), -1..1.
fn nz2(p: vec2<f32>) -> f32 {
    return 0.5 * noise1(p.x + 1.7 * noise1(p.y * 0.9 + 3.1)) + 0.5 * noise1(p.y * 1.3 + 1.3 * noise1(p.x * 1.1 + 7.7));
}

// The ray against the horizontal plane `y` blocks over the shrine: (x, z) from the shrine, 1 if hit in front of the scene.
fn sh_plane(rd: vec3<f32>, tmax: f32, base: vec3<f32>, y: f32) -> vec3<f32> {
    if (abs(rd.y) < 1e-4) { return vec3<f32>(0.0); }
    let t = (base.y + y) / rd.y;
    if (t <= 0.0 || t > tmax) { return vec3<f32>(0.0); }
    let p = rd * t - base;
    return vec3<f32>(p.x, p.z, 1.0);
}

// The ray against any plane through c with normal n: the hit point from c, w = 1 if hit in front of the scene.
fn sh_tplane(rd: vec3<f32>, tmax: f32, c: vec3<f32>, n: vec3<f32>) -> vec4<f32> {
    let den = dot(rd, n);
    if (abs(den) < 1e-4) { return vec4<f32>(0.0); }
    let t = dot(c, n) / den;
    if (t <= 0.0 || t > tmax) { return vec4<f32>(0.0); }
    return vec4<f32>(rd * t - c, 1.0);
}

// Distance from the view ray to the segment a..b: (distance, s along it, 1 if not behind the scene).
fn sh_seg(rd: vec3<f32>, tmax: f32, a: vec3<f32>, b: vec3<f32>) -> vec3<f32> {
    let q = ray_seg(rd, 100000.0, a, b);
    let d = length(q.xyz - (a + (b - a) * q.w));
    return vec3<f32>(d, q.w, 1.0 - smoothstep(tmax - 0.8, tmax + 0.8, length(q.xyz)));
}

// A vertical axis from a, h high: (distance, height along it, visible, angle round it).
fn sh_vax(rd: vec3<f32>, tmax: f32, a: vec3<f32>, h: f32) -> vec4<f32> {
    let b = a + vec3<f32>(0.0, max(h, 0.01), 0.0);
    let q = ray_seg(rd, 100000.0, a, b);
    let off = q.xyz - (a + (b - a) * q.w);
    let vis = 1.0 - smoothstep(tmax - 0.8, tmax + 0.8, length(q.xyz));
    return vec4<f32>(length(off), q.w * h, vis, atan2(off.z, off.x));
}

// A glowing point: (core, halo).
fn sh_pt(rd: vec3<f32>, tmax: f32, p: vec3<f32>, r: f32) -> vec2<f32> {
    let t = dot(p, rd);
    if (t < 0.0 || t > tmax + r) { return vec2<f32>(0.0); }
    let h = length(rd * t - p);
    return vec2<f32>(exp(-sq(h / r)), 1.0 / (1.0 + sq(h / (r * 5.0))));
}

// The ray against a vertical cylinder of radius r round the shrine's axis: (near t, far t), -1 when missed.
fn sh_cyl(rd: vec3<f32>, base: vec3<f32>, r: f32) -> vec2<f32> {
    let o = -base.xz;
    let d = rd.xz;
    let a = dot(d, d);
    if (a < 1e-6) { return vec2<f32>(-1.0); }
    let b = dot(o, d);
    let c = dot(o, o) - r * r;
    let disc = b * b - a * c;
    if (disc < 0.0) { return vec2<f32>(-1.0); }
    let s = sqrt(disc);
    return vec2<f32>((-b - s) / a, (-b + s) / a);
}

// A sheet of weather on a cylinder (u round it, v up it, in blocks). Mode: 0 rain, 1 snow, 2 wind,
// 3 swarm, 4 rising wisps, 5 cloudy haze.
fn sh_sheet(mode: i32, u: f32, v: f32, time: f32, seed: f32) -> f32 {
    if (mode == 0) {
        let c = floor(u / 0.45);
        let h = hash(c * 1.37 + seed);
        let fx = fract(u / 0.45) - 0.5 - (h - 0.5) * 0.5;
        let vv = (v + time * (24.0 + 10.0 * h)) / 3.5 + hash(c * 7.7 + seed);
        let s = fract(vv);
        let on = step(0.3, hash(c * 3.1 + floor(vv) * 1.93 + seed));
        return exp(-sq(fx / 0.07)) * smoothstep(0.0, 0.45, s) * (1.0 - smoothstep(0.5, 0.58, s)) * on;
    }
    if (mode == 1) {
        let uu = u + time * 7.0 + 0.6 * sin(v * 0.7 + time);
        let vv = v + time * 2.2;
        let c = floor(vec2<f32>(uu, vv) / 1.1);
        let f = fract(vec2<f32>(uu, vv) / 1.1) - vec2<f32>(0.5);
        let h = hash(c.x * 3.7 + c.y * 11.3 + seed);
        let j = vec2<f32>(hash(h * 17.0) - 0.5, hash(h * 29.0) - 0.5) * 0.6;
        return step(0.35, h) * exp(-sq(length(f - j) / 0.07));
    }
    if (mode == 2) {
        let c = floor(v / 0.4);
        let h = hash(c * 2.11 + seed);
        let fy = fract(v / 0.4) - 0.5;
        let uu = (u - time * (26.0 + 14.0 * h)) / 7.0 + hash(c * 5.3 + seed);
        let s = fract(uu);
        let on = step(0.45, hash(c * 1.7 + floor(uu) * 3.9 + seed));
        return exp(-sq(fy / 0.06)) * smoothstep(0.0, 0.5, s) * (1.0 - smoothstep(0.55, 0.62, s)) * on;
    }
    if (mode == 3) {
        let buzz = floor(time * 14.0);
        let uu = u + time * 5.0;
        let c = floor(vec2<f32>(uu, v) / 0.7);
        let f = fract(vec2<f32>(uu, v) / 0.7) - vec2<f32>(0.5);
        let h = hash(c.x * 5.1 + c.y * 9.7 + seed);
        let j = vec2<f32>(hash(h * 13.0 + buzz) - 0.5, hash(h * 31.0 + buzz) - 0.5) * 0.5;
        return step(0.45, h) * exp(-sq(length(f - j) / 0.08));
    }
    if (mode == 4) {
        let vv = v - time * 2.4;
        let uu = u + 0.8 * sin(vv * 0.4 + u * 0.1);
        let c = floor(vec2<f32>(uu, vv) / 2.2);
        let f = fract(vec2<f32>(uu, vv) / 2.2) - vec2<f32>(0.5);
        let h = hash(c.x * 4.3 + c.y * 7.9 + seed);
        let j = vec2<f32>(hash(h * 11.0) - 0.5, hash(h * 23.0) - 0.5) * 0.4;
        let e = (f - j) * vec2<f32>(1.0, 0.45);
        return step(0.55, h) * exp(-sq(length(e) / 0.12));
    }
    return smoothstep(-0.1, 0.7, nz2(vec2<f32>(u * 0.12 + time * 0.25, v * 0.2 - time * 0.1) + vec2<f32>(seed)));
}

// Weather round the shrine on four nested cylinders (r0 to r1, y0 to y1 over the shrine): (light, dimming).
fn sh_weather(rd: vec3<f32>, tmax: f32, base: vec3<f32>, time: f32, seed: f32, mode: i32, amt: f32, r0: f32, r1: f32, y0: f32, y1: f32) -> vec2<f32> {
    if (amt <= 0.0) { return vec2<f32>(0.0); }
    var lit = 0.0;
    var chord = 0.0;
    for (var j = 0; j < 4; j = j + 1) {
        let fj = f32(j);
        let r = mix(r0, r1, (fj + 0.5) / 4.0);
        let tc = sh_cyl(rd, base, r);
        for (var e = 0; e < 2; e = e + 1) {
            let t = select(tc.y, tc.x, e == 0);
            if (t > 0.0 && t < tmax) {
                let p = rd * t - base;
                let yk = smoothstep(y0, y0 + 3.0, p.y) * (1.0 - smoothstep(y1 - 6.0, y1, p.y));
                if (yk > 0.0) {
                    let u = atan2(p.z, p.x) * r;
                    lit = lit + sh_sheet(mode, u, p.y, time, seed + fj * 17.0 + f32(e) * 5.0) * yk / (1.0 + t / 90.0);
                }
            }
        }
        if (j == 3) {
            let t0 = max(tc.x, 0.0);
            let t1 = min(tc.y, tmax);
            let pm = rd * (0.5 * (t0 + t1)) - base;
            let ym = smoothstep(y0, y0 + 6.0, pm.y) * (1.0 - smoothstep(y1 - 12.0, y1, pm.y));
            chord = clamp((t1 - t0) / (2.0 * r1), 0.0, 1.0) * ym;
        }
    }
    return vec2<f32>(lit * amt, chord * amt);
}

// A cloud deck over the shrine (y up, radius R): dims the sky under it, its underside faintly lit.
fn sh_cloud(rd: vec3<f32>, tmax: f32, P: ShP, y: f32, R: f32, amt: f32, tint: vec3<f32>) -> Sh {
    var o = sh0();
    if (amt <= 0.0) { return o; }
    let h = sh_plane(rd, tmax, P.base, y);
    if (h.z <= 0.0) { return o; }
    let r = length(h.xy);
    let a = atan2(h.y, h.x);
    let edge = R * (0.8 + 0.2 * noise1(a * 3.0 + P.seed)) * (0.4 + 0.6 * amt);
    let dens = (1.0 - smoothstep(edge * 0.55, edge, r)) * (0.55 + 0.45 * nz2(h.xy * 0.06 + vec2<f32>(P.time * 0.15, P.seed)));
    o.d = clamp(dens, 0.0, 1.0) * 0.62 * amt;
    o.l = tint * dens * 0.07 * amt + tint * (1.0 - smoothstep(0.0, R * 0.3, r)) * 0.1 * amt;
    return o;
}

// A jagged bolt from a to b in 8 kinks: (core, glow).
fn sh_bolt(rd: vec3<f32>, tmax: f32, a: vec3<f32>, b: vec3<f32>, seed: f32, px: f32) -> vec2<f32> {
    let ax = b - a;
    let L = length(ax);
    let dir = ax / max(L, 1e-3);
    let p1 = perp(dir);
    let p2 = cross(dir, p1);
    var prev = a;
    var best = 1e9;
    var vis = 0.0;
    for (var i = 1; i <= 8; i = i + 1) {
        let s = f32(i) / 8.0;
        var nxt = a + ax * s;
        if (i < 8) {
            let amp = L * 0.08 * sin(3.14159 * s);
            nxt = nxt + p1 * (hash(seed + f32(i) * 7.13) - 0.5) * 2.0 * amp + p2 * (hash(seed + f32(i) * 3.31 + 1.7) - 0.5) * 2.0 * amp;
        }
        let q = sh_seg(rd, tmax, prev, nxt);
        if (q.x < best) {
            best = q.x;
            vis = q.z;
        }
        prev = nxt;
    }
    return vec2<f32>(exp(-sq(best / (0.16 * px))) * vis, vis / (1.0 + sq(best / (2.2 * px))));
}

// A glowing orb (energy ball, charge, gathered light).
fn sh_orb(rd: vec3<f32>, tmax: f32, c: vec3<f32>, r: f32, col: vec3<f32>, time: f32, seed: f32, core: f32) -> Sh {
    var o = sh0();
    if (r <= 0.02) { return o; }
    let hb = ray_ball(rd, tmax, c, r);
    let h = hb.x;
    let nz = sqrt(max(1.0 - sq(h / r), 0.0));
    let disc = smoothstep(r, r * 0.92, h) * hb.y;
    let ax = normalize(cross(rd, vec3<f32>(0.0, 1.0, 0.0)) + vec3<f32>(1e-4, 0.0, 0.0));
    let ay = cross(ax, rd);
    let q = rd * max(dot(c, rd), 0.0) - c;
    let uv = vec2<f32>(dot(q, ax), dot(q, ay)) / r;
    let sw = 0.5 + 0.5 * sin(atan2(uv.y, uv.x) * 3.0 + length(uv) * 8.0 - time * 5.0 + seed);
    o.l = col * ((0.6 + 0.8 * nz + 0.5 * sw) * disc + ln(h - r, r * 0.18) * 1.2 * hb.y + 0.45 / (1.0 + sq(h / (2.5 * r))) * hb.y);
    o.w = pow(nz, 5.0) * core * disc;
    return o;
}

// Billboard coordinates (blocks) of the ray round a point c, as the camera sees it.
fn sh_bb(rd: vec3<f32>, c: vec3<f32>) -> vec2<f32> {
    let ax = normalize(cross(rd, vec3<f32>(0.0, 1.0, 0.0)) + vec3<f32>(1e-4, 0.0, 0.0));
    let ay = cross(ax, rd);
    let q = rd * max(dot(c, rd), 0.0) - c;
    return vec2<f32>(dot(q, ax), dot(q, ay));
}

// The warning on the floor every type shares: a ring closing in and the floor warming.
fn sh_floor(P: ShP, c: vec3<f32>) -> vec3<f32> {
    if (P.band <= 0.0 || P.hd > 26.0) { return vec3<f32>(0.0); }
    let k = P.k;
    let rr = mix(18.0, 3.0, k * k);
    let pulse = 0.65 + 0.35 * sin(P.time * (4.0 + 9.0 * k));
    return c * (ln(P.hd - rr, 0.25 + 0.25 * k) * (0.6 + 1.0 * k) * pulse + exp(-sq(P.hd / (1.5 + 2.5 * k))) * k * k * 1.2) * P.band;
}

// A rocky lump (boulder, debris): solid, lit from above, a coloured rim.
fn sh_rock(rd: vec3<f32>, tmax: f32, c: vec3<f32>, r: f32, rim: vec3<f32>, glow: f32) -> Sh {
    var o = sh0();
    let hb = ray_ball(rd, tmax, c, r);
    if (hb.y <= 0.0 || hb.x > r * 3.0) { return o; }
    let disc = smoothstep(r, r * 0.85, hb.x) * hb.y;
    let uv = sh_bb(rd, c) / r;
    let top = clamp(0.5 + 0.5 * uv.y, 0.0, 1.0);
    o.d = disc * 0.86;
    o.l = vec3<f32>(0.32, 0.29, 0.26) * (0.15 + 0.35 * top) * disc + rim * ln(hb.x - r, r * 0.14) * hb.y * glow;
    return o;
}

// ---- The types ----

fn sh_normal(rd: vec3<f32>, tmax: f32, P: ShP) -> Sh {
    var o = sh0();
    let k = P.k;
    let t = P.wt;
    let C = P.foc;
    let gold = vec3<f32>(1.0, 0.93, 0.78);
    let R = mix(0.6, 6.5, smoothstep(0.03, 0.75, k)) * (1.0 - 0.5 * smoothstep(0.8, 0.96, k));
    // Energy pulled in from all round, pulses running inward along each stream.
    for (var i = 0; i < 12; i = i + 1) {
        let fi = f32(i);
        let a = fi * 2.399 + P.seed;
        let el = 0.1 + 1.1 * hash(fi * 3.7 + P.seed);
        let S = P.base + vec3<f32>(cos(a) * 80.0 * cos(el), 6.0 + 70.0 * sin(el), sin(a) * 80.0 * cos(el));
        let on = smoothstep(0.02 + 0.035 * fi, 0.1 + 0.035 * fi, k) * (1.0 - smoothstep(0.9, 0.97, k));
        if (on <= 0.0) { continue; }
        let q = sh_seg(rd, tmax, S, C);
        let w = 0.3 * sh_px(C) * (0.4 + 0.8 * q.y);
        let pulse = 0.3 + 1.3 * pow(fract(q.y * 5.0 - t * 1.8 + hash(fi)), 6.0);
        o.l = o.l + mix(P.col, gold, 0.5) * (exp(-sq(q.x / w)) * pulse * 1.3 + 0.1 / (1.0 + sq(q.x / (w * 6.0)))) * on * q.z * smoothstep(0.0, 0.2, q.y);
    }
    o = sh_mix(o, sh_orb(rd, tmax, C, R, mix(P.col, gold, 0.35), P.time, P.seed, 0.6 + 0.8 * k));
    // Rings orbiting it, tilted every way.
    let ron = smoothstep(0.15, 0.4, k);
    for (var j = 0; j < 3; j = j + 1) {
        let fj = f32(j);
        let tilt = 0.5 + 0.45 * fj;
        let spin = t * (0.9 + 0.5 * fj) + fj * 2.1;
        let n = normalize(vec3<f32>(sin(tilt) * cos(spin), cos(tilt), sin(tilt) * sin(spin)));
        let h = sh_tplane(rd, tmax, C, n);
        if (h.w > 0.0) {
            let rr = length(h.xyz);
            let ringR = R * (1.7 + 0.3 * fj);
            o.l = o.l + mix(P.col, gold, 0.6) * ln(rr - ringR, 0.12 * sh_px(C)) * 1.6 * ron;
        }
    }
    o.l = o.l + sh_floor(P, mix(P.col, gold, 0.4));
    return o;
}

fn sh_water(rd: vec3<f32>, tmax: f32, P: ShP) -> Sh {
    var o = sh0();
    let t = P.wt;
    let k = P.k;
    let foam = vec3<f32>(0.85, 0.95, 1.0);
    let rain = smoothstep(0.2, 1.6, t);
    o = sh_mix(o, sh_cloud(rd, tmax, P, 56.0, 52.0, rain, P.col));
    // The downpour round the shrine.
    let wr = sh_weather(rd, tmax, P.base, P.time, P.seed, 0, rain, 4.0, 40.0, -2.0, 54.0);
    o.l = o.l + mix(P.col, foam, 0.7) * wr.x * 0.6;
    o.d = max(o.d, wr.y * 0.14);
    // The whirlpool forming over the shrine.
    let hW = 34.0;
    let Rw = mix(2.0, 22.0, smoothstep(0.08, 0.7, k));
    let on = smoothstep(0.03, 0.15, k);
    let wp = sh_plane(rd, tmax, P.base, hW);
    if (wp.z > 0.0) {
        let r = length(wp.xy);
        if (r < Rw * 1.3) {
            let a = atan2(wp.y, wp.x);
            let spin = t * (1.5 + 5.0 * k);
            let arm = pow(0.5 + 0.5 * sin(3.0 * a + 7.0 * log(r + 1.0) - spin), 3.0);
            let fine = 0.5 + 0.5 * sin(9.0 * a + 16.0 * log(r + 1.0) - spin * 1.7);
            let edge = 1.0 - smoothstep(Rw * 0.8, Rw * 1.25, r);
            let eye = smoothstep(Rw * 0.06, Rw * 0.2, r);
            o.l = o.l + (P.col * (0.35 + 0.9 * arm) + foam * pow(arm, 4.0) * fine * 0.9) * edge * eye * on;
            o.w = o.w + pow(arm * fine, 6.0) * 0.4 * edge * eye * on;
            o.d = max(o.d, (edge * (1.0 - eye) * 0.7 + edge * 0.3) * on);
        }
    }
    // The waterspout reaching down from its eye to the shrine.
    let ext = smoothstep(0.35, 0.95, k);
    if (ext > 0.0) {
        let tipY = mix(hW, 0.5, ext);
        let q = sh_vax(rd, tmax, P.base + vec3<f32>(0.0, tipY, 0.0), hW - tipY);
        let s = q.y / max(hW - tipY, 0.01);
        let rf = (0.7 + 6.5 * pow(s, 2.2)) * (1.0 + 0.12 * noise1(q.y * 0.6 - t * 6.0 + P.seed)) * (1.0 + P.camd / 200.0);
        let swirl = 0.5 + 0.5 * sin(4.0 * q.w + q.y * 0.7 - t * 9.0);
        let shell = ln(q.x - rf, 0.45 + 0.15 * rf);
        let fill = exp(-sq(q.x / rf));
        o.l = o.l + (P.col * (shell * 1.1 + fill * 0.35) * (0.6 + 0.6 * swirl) + foam * shell * pow(swirl, 4.0) * 0.7) * q.z;
        o.d = max(o.d, fill * 0.3 * q.z);
    }
    o.l = o.l + sh_floor(P, mix(P.col, foam, 0.3));
    return o;
}

fn el_time(i: i32, F: f32) -> f32 { return F * pow(f32(i) / 12.0, 0.6); }

fn sh_electric(rd: vec3<f32>, tmax: f32, P: ShP) -> Sh {
    var o = sh0();
    let t = P.wt;
    let k = P.k;
    let pale = vec3<f32>(0.86, 0.92, 1.0);
    o = sh_mix(o, sh_cloud(rd, tmax, P, 72.0, 72.0, smoothstep(0.0, 1.2, t), mix(P.col, pale, 0.5)));
    let top = P.base + vec3<f32>(0.0, 2.0, 0.0);
    let px = sh_px(top);
    var count = 0.0;
    // Strike after strike out of the cloud onto the shrine, faster and faster (fx.rs `strike_times`).
    for (var i = 1; i < 12; i = i + 1) {
        let dt = t - el_time(i, P.fall);
        if (dt < 0.0) { break; }
        count = count + 1.0;
        if (dt > 0.5) { continue; }
        let fi = f32(i);
        let a = fi * 2.399 + P.seed * 0.1;
        let rr = 20.0 + 16.0 * hash(fi + P.seed);
        let S = P.base + vec3<f32>(cos(a) * rr, 68.0 + 10.0 * hash(fi * 1.7), sin(a) * rr);
        // `/capifx flash off` (u.info.z): no restrike spike, and the halo and cloud glow stay low, so
        // the quickening strikes never flicker a wide area.
        let nf = clamp(u.info.z, 0.0, 1.0);
        let env = exp(-dt * 8.0) + 0.5 * exp(-sq((dt - 0.09) / 0.03)) * (1.0 - nf);
        let soft = 1.0 - 0.7 * nf;
        let b = sh_bolt(rd, tmax, S, top, fi * 13.7 + P.seed, px);
        o.l = o.l + (pale * b.x * 2.4 + P.col * b.y * 0.7 * soft) * env;
        o.w = o.w + b.x * env * 0.8 * soft;
        // The cloud lights from inside where it came from.
        let cp = sh_pt(rd, tmax, S, 14.0);
        o.l = o.l + P.col * cp.x * 0.35 * env * soft;
    }
    // The charge it builds: a crackling plasma ball on the shrine, arcs leaping off it to the ground.
    let Rc = (0.6 + 0.32 * count) * (1.0 + 0.4 * smoothstep(0.9, 1.0, k));
    let cc = P.base + vec3<f32>(0.0, 1.0 + Rc, 0.0);
    o = sh_mix(o, sh_orb(rd, tmax, cc, Rc, mix(P.col, pale, 0.4), P.time * 3.0, P.seed, 0.8));
    let flick = floor(P.time * 11.0);
    for (var j = 0; j < 3; j = j + 1) {
        let fj = f32(j);
        let a = hash(flick * 1.3 + fj * 7.0 + P.seed) * 6.2831853;
        let reach = Rc * 1.4 + 2.0 + 5.0 * hash(flick + fj * 3.0);
        let end = P.base + vec3<f32>(cos(a) * reach, 0.1, sin(a) * reach);
        let b = sh_bolt(rd, tmax, cc, end, flick * 3.1 + fj * 9.0, px * 0.6);
        o.l = o.l + (pale * b.x * 1.6 + P.col * b.y * 0.3) * smoothstep(1.0, 4.0, count);
    }
    o.l = o.l + sh_floor(P, P.col);
    return o;
}

fn sh_dark(rd: vec3<f32>, tmax: f32, P: ShP) -> Sh {
    var o = sh0();
    let t = P.wt;
    let k = P.k;
    let rim = mix(vec3<f32>(0.85, 0.08, 0.22), P.col, 0.25);
    // The sky dims round the shrine, slowly (never a flash).
    o.d = 0.42 * smoothstep(0.4, 4.5, t) / (1.0 + sq(P.camd / 110.0));
    // Shadows creeping in over the ground from all sides.
    if (P.band > 0.0 && P.hd < 40.0) {
        let rt = 36.0 * (1.0 - smoothstep(0.02, 0.62, k));
        var cover = 0.0;
        var edge = 0.0;
        for (var j = 0; j < 9; j = j + 1) {
            let fj = f32(j);
            let aj = fj * 0.6981 + 0.22 * sin(P.hd * 0.25 + t * 1.3 + fj * 2.0) + P.seed;
            var da = P.gang - aj;
            da = da - 6.2831853 * floor(da / 6.2831853 + 0.5);
            let grow = smoothstep(rt, rt + 6.0, P.hd);
            let width = (0.8 + 0.11 * P.hd) * grow;
            let ad = abs(da) * P.hd;
            cover = max(cover, (1.0 - smoothstep(width * 0.55, width, ad)) * grow);
            edge = max(edge, ln(ad - width * 0.8, 0.3) * grow);
        }
        let fade = 1.0 - smoothstep(32.0, 40.0, P.hd);
        o.d = max(o.d, cover * 0.82 * fade * P.band);
        o.l = o.l + rim * edge * 0.7 * fade * P.band;
    }
    // ...and climbing up round the shrine.
    let climb = 16.0 * smoothstep(0.5, 0.94, k);
    if (climb > 0.1) {
        let tc = sh_cyl(rd, P.base, 3.4);
        for (var e = 0; e < 2; e = e + 1) {
            let tt = select(tc.y, tc.x, e == 0);
            if (tt > 0.0 && tt < tmax) {
                let p = rd * tt - P.base;
                let a = atan2(p.z, p.x);
                let lane = floor(a / 6.2831853 * 9.0 + 0.5);
                let tip = climb * (0.6 + 0.4 * hash(lane + P.seed)) + 1.2 * sin(t * 2.0 + lane);
                let fing = smoothstep(0.25, 0.55, 0.5 + 0.5 * cos(a * 9.0 + p.y * 0.25));
                let below = 1.0 - smoothstep(tip - 0.6, tip, p.y);
                o.d = max(o.d, fing * below * 0.85 * step(-1.0, p.y));
                o.l = o.l + rim * fing * ln(p.y - tip, 0.4) * 1.2;
            }
        }
    }
    // The eclipse: a black sun over the shrine, its corona flaring.
    let ER = 8.0 * smoothstep(0.08, 0.42, k) * (1.0 - 0.6 * smoothstep(0.86, 1.0, k));
    if (ER > 0.05) {
        let hb = ray_ball(rd, tmax, P.foc, ER);
        let h = hb.x;
        let uv = sh_bb(rd, P.foc);
        let a = atan2(uv.y, uv.x);
        let streak = 0.5 + 0.5 * noise1(a * 4.0 + t * 0.7 + P.seed) + 0.3 * noise1(a * 11.0 - t * 1.3);
        let out = smoothstep(ER * 0.97, ER * 1.01, h);
        let cor = ln(h - ER, ER * 0.06) * 1.8 + (1.0 / (1.0 + sq((h - ER) / (ER * (0.25 + 0.3 * streak))))) * 0.9;
        o.l = o.l + mix(rim, vec3<f32>(1.0, 0.85, 0.9), 0.25) * cor * out * hb.y;
        o.w = o.w + ln(h - ER, ER * 0.025) * 0.5 * hb.y;
        o.d = max(o.d, smoothstep(ER, ER * 0.97, h) * 0.96 * hb.y);
    }
    o.l = o.l + sh_floor(P, rim);
    return o;
}

// The dragon's flight: two circles high over the shrine, then up and over into the dive (fx.rs `dragon_at`).
fn dragon_at(base: vec3<f32>, seed: f32, s: f32, F: f32) -> vec3<f32> {
    let T1 = F * 0.7;
    let a0 = seed * 0.0174533;
    let w = 1.3;
    let st = min(s, T1);
    let th = a0 + st * w;
    let P1 = base + vec3<f32>(cos(th) * 42.0, 50.0 + 4.0 * sin(st * 1.7), sin(th) * 42.0);
    if (s <= T1) { return P1; }
    let u = clamp((s - T1) / (F - T1), 0.0, 1.0);
    let e = mix(u, u * u, 0.6);
    let C1 = P1 + vec3<f32>(-sin(th) * 30.0, 30.0, cos(th) * 30.0);
    let C2 = base + vec3<f32>(0.0, 85.0, 0.0);
    let E = base + vec3<f32>(0.0, 1.0, 0.0);
    let m = 1.0 - e;
    return P1 * (m * m * m) + C1 * (3.0 * m * m * e) + C2 * (3.0 * m * e * e) + E * (e * e * e);
}

fn sh_dragon(rd: vec3<f32>, tmax: f32, P: ShP) -> Sh {
    var o = sh0();
    let t = P.wt;
    let F = P.fall;
    let appear = smoothstep(0.0, 0.8, t);
    let fire = mix(P.col, vec3<f32>(1.0, 0.55, 0.9), 0.3);
    let dive = smoothstep(F * 0.72, F * 0.8, t);
    // Its body: a chain of segments along where it has just flown, thick at the shoulders.
    var best = 1e9;
    var bu = 0.0;
    var bvis = 0.0;
    var prev = dragon_at(P.base, P.seed, t, F);
    let head = prev;
    let px = sh_px(head);
    for (var i = 1; i <= 12; i = i + 1) {
        let fi = f32(i);
        let nxt = dragon_at(P.base, P.seed, t - fi * 0.075, F);
        let q = sh_seg(rd, tmax, prev, nxt);
        let u = (fi - 1.0 + q.y) / 12.0;
        let rad = (0.9 + 2.0 * smoothstep(0.0, 0.18, u)) * (1.0 - 0.85 * smoothstep(0.25, 1.0, u)) * 1.9;
        let sd = q.x - rad;
        if (sd < best) {
            best = sd;
            bu = u;
            bvis = q.z;
        }
        prev = nxt;
    }
    let body = smoothstep(0.35 * px, -0.25, best) * bvis * appear;
    let scales = pow(hash(floor(bu * 60.0) + P.seed), 6.0) * (0.5 + 0.5 * sin(P.time * 6.0 + bu * 30.0));
    o.d = body * 0.78;
    o.l = fire * (ln(best, 0.35 * px) * 1.6 * bvis + body * (0.12 + 0.8 * scales) + 0.3 / (1.0 + sq(max(best, 0.0) / (3.0 * px))) * bvis) * appear;
    // A streaming trail of light behind it.
    var tp = prev;
    for (var i = 1; i <= 5; i = i + 1) {
        let nxt = dragon_at(P.base, P.seed, t - 0.9 - f32(i) * 0.1, F);
        let q = sh_seg(rd, tmax, tp, nxt);
        let fade = 1.0 - (f32(i - 1) + q.y) / 5.0;
        o.l = o.l + fire * exp(-sq(q.x / (0.5 * px * (0.4 + fade)))) * fade * 0.8 * q.z * appear;
        tp = nxt;
    }
    // Wings from the shoulders, beating; folded back in the dive.
    let s1 = dragon_at(P.base, P.seed, t - 0.08, F);
    let s3 = dragon_at(P.base, P.seed, t - 0.23, F);
    let fwd = normalize(s1 - s3 + vec3<f32>(0.0, 1e-4, 0.0));
    let right = perp(fwd);
    let up2 = cross(right, fwd);
    let flap = mix(0.6 * sin(t * 5.5), -0.9, dive);
    let span = 26.0 * (1.0 - 0.55 * dive);
    let sh = mix(s1, s3, 0.5);
    for (var side = 0; side < 2; side = side + 1) {
        let sg = select(-1.0, 1.0, side == 0);
        let wd = normalize(right * sg * cos(flap) + up2 * sin(flap) - fwd * 0.6 * dive);
        let n = normalize(cross(wd, fwd));
        let h = sh_tplane(rd, tmax, sh, n);
        if (h.w > 0.0) {
            let x = dot(h.xyz, wd) / span;
            let y = dot(h.xyz, fwd);
            if (x > -0.05 && x < 1.05) {
                let xc = clamp(x, 0.0, 1.0);
                let front = 1.5 + 3.4 * (1.0 - xc);
                let back = -11.0 * pow(1.0 - xc, 0.7) - 1.4 * (0.5 + 0.5 * cos(xc * 18.85));
                let inside = smoothstep(-0.02, 0.02, x) * (1.0 - smoothstep(0.98, 1.02, x)) * smoothstep(back, back + 0.3, y) * (1.0 - smoothstep(front - 0.3, front, y));
                var bones = ln(y - front, 0.22 * px);
                for (var b = 1; b <= 3; b = b + 1) {
                    bones = max(bones, ln(y + 11.0 * pow(1.0 - xc, 0.7) * f32(b) / 3.4, 0.16 * px) * inside);
                }
                o.d = max(o.d, inside * 0.6 * appear);
                o.l = o.l + fire * (bones * 1.3 + inside * 0.12 + ln(y - back, 0.18 * px) * 0.8 * step(0.0, x)) * appear;
            }
        }
    }
    // Eyes and the glow of its breath.
    let eye = sh_pt(rd, tmax, head + fwd * 1.6 + right * 1.1 + up2 * 0.8, 0.3 * px).x + sh_pt(rd, tmax, head + fwd * 1.6 - right * 1.1 + up2 * 0.8, 0.3 * px).x;
    o.l = o.l + vec3<f32>(1.0, 0.85, 0.5) * eye * 2.5 * appear;
    o.w = o.w + eye * 0.8 * appear;
    o.l = o.l + sh_floor(P, fire);
    return o;
}

fn pixie_at(i: i32, P: ShP, s: f32) -> vec3<f32> {
    let fi = f32(i);
    let h1 = hash(fi * 1.31 + P.seed);
    let h2 = hash(fi * 2.17 + P.seed);
    let h3 = hash(fi * 3.73 + P.seed);
    let h4 = hash(fi * 5.29 + P.seed);
    let kk = s / P.fall;
    let e = smoothstep(0.05 + 0.35 * h3, 0.88, kk);
    let r = mix(26.0 + 32.0 * h1, 1.5 + 2.0 * h4, e);
    let a = fi * 2.399 + s * (0.5 + 0.6 * h4) + e * 9.0;
    let y = mix(4.0 + 36.0 * h2, 16.0, e) + sin(s * 3.0 + fi) * 2.0 * (1.0 - e) + sin(a * 2.0) * 0.8 * e;
    let p = P.base + vec3<f32>(cos(a) * r, y, sin(a) * r);
    return mix(p, P.base + vec3<f32>(0.0, 1.0, 0.0), smoothstep(0.93, 1.0, kk));
}

fn sh_fairy(rd: vec3<f32>, tmax: f32, P: ShP) -> Sh {
    var o = sh0();
    let t = P.wt;
    let k = P.k;
    let white = vec3<f32>(1.0, 0.97, 1.0);
    // Glittering dust drifting up all round.
    let wr = sh_weather(rd, tmax, P.base, P.time, P.seed, 4, smoothstep(0.0, 1.5, t) * (1.0 - smoothstep(0.92, 1.0, k)), 5.0, 36.0, -1.0, 34.0);
    o.l = o.l + mix(P.col, white, 0.5) * wr.x * 0.9;
    // Pixies streaming in from everywhere, spiralling into a gathering over the shrine.
    for (var i = 0; i < 32; i = i + 1) {
        let p0 = pixie_at(i, P, t);
        let p1 = pixie_at(i, P, t - 0.14);
        let q = sh_seg(rd, tmax, p0, p1);
        let px = sh_px(p0);
        let tw = 0.6 + 0.4 * sin(P.time * 14.0 + f32(i) * 3.0);
        let m = i % 4;
        var c = P.col;
        if (m == 1) { c = white; }
        if (m == 2) { c = vec3<f32>(0.78, 0.7, 1.0); }
        if (m == 3) { c = vec3<f32>(0.7, 1.0, 0.86); }
        let head = 1.0 - q.y;
        o.l = o.l + c * (exp(-sq(q.x / (0.16 * px))) * (0.3 + 1.6 * head * head) * tw + 0.22 / (1.0 + sq(q.x / (0.9 * px))) * head) * q.z * smoothstep(0.0, 0.5, t);
        o.w = o.w + exp(-sq(q.x / (0.07 * px))) * head * head * 0.6 * q.z;
    }
    // The bloom of light they gather into.
    let B = P.foc;
    let gr = smoothstep(0.4, 0.9, k) * (1.0 - smoothstep(0.93, 1.0, k));
    if (gr > 0.0) {
        let uv = sh_bb(rd, B);
        let r = length(uv);
        let a = atan2(uv.y, uv.x) + t * 0.6;
        let R = 6.0 * gr;
        let petal = R * (0.4 + 0.6 * abs(cos(2.5 * a)));
        let vis = select(0.0, 1.0, dot(B, rd) < tmax);
        o.l = o.l + mix(P.col, white, 0.3) * (ln(r - petal, 0.25 * sh_px(B)) * 1.4 + (1.0 - smoothstep(petal * 0.6, petal, r)) * 0.35 + exp(-sq(r / (R * 0.3))) * 1.2) * gr * vis;
        o.w = o.w + exp(-sq(r / (R * 0.18))) * gr * vis;
    }
    o.l = o.l + sh_floor(P, mix(P.col, white, 0.3));
    return o;
}

fn sh_psychic(rd: vec3<f32>, tmax: f32, P: ShP) -> Sh {
    var o = sh0();
    let t = P.wt;
    let k = P.k;
    let rim = mix(P.col, vec3<f32>(1.0, 0.7, 0.95), 0.3);
    let pull = smoothstep(0.85, 1.0, k);
    // Rubble lifting off the ground and drifting round it.
    for (var i = 0; i < 12; i = i + 1) {
        let fi = f32(i);
        let h1 = hash(fi * 1.7 + P.seed);
        let h2 = hash(fi * 2.9 + P.seed);
        let h3 = hash(fi * 4.1 + P.seed);
        let lift = smoothstep(0.03 + 0.3 * h3, 0.3 + 0.3 * h3, k);
        let a = fi * 2.399 + t * (0.12 + 0.12 * h2);
        let r = (8.0 + 20.0 * h1) * (1.0 - 0.9 * pull);
        let y = mix(mix(-0.4, 3.0 + 18.0 * h2, lift) + sin(t * 1.3 + fi) * 0.6 * lift, 1.0, pull);
        o = sh_mix(o, sh_rock(rd, tmax, P.base + vec3<f32>(cos(a) * r, y, sin(a) * r), 0.5 + 0.8 * hash(fi * 7.3), rim, 1.2));
    }
    // Rings warping the air round it, tilted and wobbling.
    for (var j = 0; j < 4; j = j + 1) {
        let fj = f32(j);
        let on = smoothstep(0.08 + 0.12 * fj, 0.3 + 0.12 * fj, k) * (1.0 - pull);
        if (on <= 0.0) { continue; }
        let c = P.base + vec3<f32>(0.0, 3.0 + 7.0 * fj, 0.0);
        let sp = t * (0.4 + 0.25 * fj) * select(-1.0, 1.0, j % 2 == 0);
        let n = normalize(vec3<f32>(0.35 * cos(sp), 1.0, 0.35 * sin(sp)));
        let h = sh_tplane(rd, tmax, c, n);
        if (h.w > 0.0) {
            let rr = length(h.xyz);
            let a = atan2(h.z, h.x);
            let R = (10.0 + 3.0 * fj) * (1.0 + 0.05 * sin(5.0 * a - t * 3.0 + fj));
            o.l = o.l + rim * (ln(rr - R, 0.14 * sh_px(c)) * 1.4 + ln(rr - R * 0.92, 0.08 * sh_px(c)) * 0.6) * on;
        }
    }
    // The eye in the sky: it opens, looks at you, then shuts to a point and falls in.
    let E = P.foc;
    let open = smoothstep(0.12, 0.38, k) * (1.0 - smoothstep(0.82, 0.92, k));
    let size = 1.0 - 0.8 * smoothstep(0.86, 1.0, k);
    if (open > 0.0 || size < 1.0) {
        let uv = sh_bb(rd, E) / max(size, 0.05);
        let W = 14.0;
        let H = 7.0 * open + 0.25;
        let xu = clamp(abs(uv.x) / W, 0.0, 1.0);
        let lid = H * pow(1.0 - xu * xu, 0.8);
        let vis = select(0.0, 1.0, dot(E, rd) < tmax) * step(abs(uv.x), W * 1.05);
        let inside = 1.0 - smoothstep(lid - 0.25, lid + 0.15, abs(uv.y));
        let px = sh_px(E);
        o.l = o.l + rim * ln(abs(uv.y) - lid, 0.3 * px) * 1.8 * vis;
        let ri = 4.6;
        let rr = length(uv);
        let a = atan2(uv.y, uv.x);
        let iris = (1.0 - smoothstep(ri - 0.3, ri, rr)) * inside;
        let fib = 0.6 + 0.4 * sin(a * 24.0 + rr * 2.0 + P.seed);
        let pupil = (1.0 - smoothstep(0.7 + 0.5 * pull, 1.0 + 0.5 * pull, abs(uv.x) / max(1.0 - rr / (ri * 1.2), 0.05))) * iris;
        o.l = o.l + (rim * (iris * fib * 1.3 + ln(rr - ri, 0.2 * px) * 1.5 * inside) + vec3<f32>(1.0, 0.85, 0.95) * inside * 0.12) * vis;
        o.w = o.w + ln(rr - ri * 0.55, 0.12 * px) * inside * 0.4 * vis;
        o.d = max(o.d, (inside * 0.55 + pupil * 0.45) * vis);
    }
    o.l = o.l + sh_floor(P, rim);
    return o;
}

fn sh_ghost(rd: vec3<f32>, tmax: f32, P: ShP) -> Sh {
    var o = sh0();
    let t = P.wt;
    let k = P.k;
    let soul = mix(P.col, vec3<f32>(0.55, 0.85, 1.0), 0.35);
    o.d = 0.22 * smoothstep(0.5, 4.0, t) / (1.0 + sq(P.camd / 110.0));
    // Spirits rising out of the ground all round.
    let wr = sh_weather(rd, tmax, P.base, P.time, P.seed, 4, smoothstep(0.0, 1.6, t), 6.0, 34.0, -1.0, 30.0);
    o.l = o.l + soul * wr.x * 1.1;
    // Bigger wisps spiralling up into the gate, long tails behind them.
    let G = P.foc;
    for (var i = 0; i < 14; i = i + 1) {
        let fi = f32(i);
        let h1 = hash(fi * 1.9 + P.seed);
        let start = 0.4 * hash(fi * 3.3 + P.seed);
        let u0 = clamp((k - start) / (1.0 - start), 0.0, 1.0);
        let u1 = clamp((k - 0.05 - start) / (1.0 - start), 0.0, 1.0);
        let a0 = fi * 2.399 + u0 * 5.0;
        let a1 = fi * 2.399 + u1 * 5.0;
        let r0 = (16.0 + 18.0 * h1) * (1.0 - u0);
        let r1 = (16.0 + 18.0 * h1) * (1.0 - u1);
        let p0 = mix(P.base + vec3<f32>(cos(a0) * r0, 0.5, sin(a0) * r0), G, u0 * u0);
        let p1 = mix(P.base + vec3<f32>(cos(a1) * r1, 0.5, sin(a1) * r1), G, u1 * u1);
        let q = sh_seg(rd, tmax, p0, p1);
        let head = 1.0 - q.y;
        let on = step(start, k) * (1.0 - smoothstep(0.9, 1.0, u0));
        let px = sh_px(p0);
        o.l = o.l + soul * (exp(-sq(q.x / (0.35 * px * (0.3 + head)))) * (0.2 + 1.2 * head) + 0.15 / (1.0 + sq(q.x / (1.5 * px)))) * q.z * on;
    }
    // The spectral gate: a ring of runes opening on the dark, then tipping over onto the shrine.
    let Rg = 11.0 * smoothstep(0.12, 0.55, k) * (1.0 - 0.45 * smoothstep(0.86, 1.0, k));
    if (Rg > 0.05) {
        let fl = normalize(vec3<f32>(G.x, 0.0, G.z) + vec3<f32>(1e-4, 0.0, 0.0));
        let tilt = smoothstep(0.78, 0.94, k);
        let n = normalize(mix(fl, vec3<f32>(0.0, 1.0, 0.0), tilt));
        let h = sh_tplane(rd, tmax, G, n);
        if (h.w > 0.0) {
            let e1 = perp(n);
            let e2 = cross(n, e1);
            let x = dot(h.xyz, e1);
            let y = dot(h.xyz, e2);
            let r = length(vec2<f32>(x, y));
            let a = atan2(y, x) + t * 0.5;
            let px = sh_px(G);
            let cell = a / 6.2831853 * 18.0;
            let band = clamp((r - Rg * 0.8) / (Rg * 0.18), 0.0, 1.0);
            let runes = rune(fract(cell), band, hash(floor(cell) + P.seed)) * step(Rg * 0.8, r) * step(r, Rg * 0.98);
            o.l = o.l + soul * (ln(r - Rg, 0.3 * px) * 1.8 + ln(r - Rg * 0.8, 0.16 * px) * 1.0 + runes * 1.4);
            let inner = 1.0 - smoothstep(Rg * 0.76, Rg * 0.82, r);
            let spiral = pow(0.5 + 0.5 * sin(2.0 * a + 4.0 * log(r + 1.0) - t * 3.0), 5.0);
            o.l = o.l + soul * spiral * inner * 0.45;
            o.d = max(o.d, inner * 0.82);
        }
    }
    o.l = o.l + sh_floor(P, soul);
    return o;
}

fn vine_at(j: i32, s: f32, P: ShP) -> vec3<f32> {
    let fj = f32(j);
    let a = fj * 1.0472 + P.seed * 0.01 + s * 2.6;
    let r = 18.0 * (1.0 - s) + 3.5 * s + 4.0 * sin(s * 3.14159);
    return P.base + vec3<f32>(cos(a) * r, 30.0 * s + 2.0 * sin(s * 6.0 + fj), sin(a) * r);
}

fn sh_grass(rd: vec3<f32>, tmax: f32, P: ShP) -> Sh {
    var o = sh0();
    let t = P.wt;
    let k = P.k;
    let leaf = mix(P.col, vec3<f32>(0.7, 1.0, 0.4), 0.3);
    let bloom = vec3<f32>(1.0, 0.75, 0.9);
    // Roots glowing out under the ground.
    if (P.band > 0.0) {
        let c = zcracks(P.hd, P.gang, 30.0 * smoothstep(0.0, 0.5, k), P.seed);
        o.l = o.l + leaf * c * 1.3 * P.band;
    }
    // Giant vines bursting out round the shrine, twisting up into one crown.
    for (var j = 0; j < 6; j = j + 1) {
        let g = smoothstep(0.02 + 0.05 * f32(j), 0.62 + 0.03 * f32(j), k);
        if (g <= 0.0) { continue; }
        var prev = vine_at(j, 0.0, P);
        for (var m = 1; m <= 8; m = m + 1) {
            let s1 = g * f32(m) / 8.0;
            let nxt = vine_at(j, s1, P);
            let q = sh_seg(rd, tmax, prev, nxt);
            let sa = g * (f32(m - 1) + q.y) / 8.0;
            let th = mix(1.7, 0.6, sa) * (1.0 - smoothstep(g - 0.06, g, sa) * 0.6);
            let sd = q.x - th;
            let px = sh_px(nxt);
            let body = smoothstep(0.2 * px, -0.2, sd) * q.z;
            o.d = max(o.d, body * 0.8);
            let pulse = pow(0.5 + 0.5 * sin(sa * 40.0 - t * 6.0 + f32(j)), 8.0);
            o.l = o.l + leaf * (ln(sd, 0.22 * px) * 0.55 * q.z + body * (0.06 + 0.45 * pulse));
            prev = nxt;
        }
    }
    // The bud at the top, swelling, then unfolding into a flower.
    let B = P.base + vec3<f32>(0.0, 33.0, 0.0);
    let Rb = 6.5 * smoothstep(0.45, 0.8, k);
    let open = smoothstep(0.84, 0.98, k);
    if (Rb > 0.05) {
        let hb = ray_ball(rd, tmax, B, Rb * (1.0 - 0.6 * open));
        let disc = smoothstep(Rb, Rb * 0.9, hb.x) * hb.y * (1.0 - open);
        let uv = sh_bb(rd, B);
        let seam = ln(abs(fract(atan2(uv.y, uv.x) / 6.2831853 * 5.0) - 0.5) * length(uv), 0.15);
        o.d = max(o.d, disc * 0.7);
        o.l = o.l + mix(leaf, bloom, 0.5) * (disc * (0.15 + seam * 0.8) + ln(hb.x - Rb, 0.25 * sh_px(B)) * 1.4 * hb.y * (1.0 - open)) + bloom * 0.5 / (1.0 + sq(hb.x / Rb)) * hb.y * smoothstep(0.6, 0.85, k);
        let h = sh_plane(rd, tmax, P.base, 33.0);
        if (h.z > 0.0 && open > 0.0) {
            let r = length(h.xy);
            let a = atan2(h.y, h.x) + t * 0.3;
            let pr = Rb * 2.6 * open * (0.35 + 0.65 * abs(cos(2.5 * a)));
            let fill = 1.0 - smoothstep(pr * 0.8, pr, r);
            o.l = o.l + bloom * (fill * 0.6 + ln(r - pr, 0.25) * 1.3) * open + P.hot * exp(-sq(r / 2.0)) * 1.5 * open;
            o.w = o.w + exp(-sq(r / 1.0)) * open;
        }
    }
    o.l = o.l + sh_floor(P, leaf);
    return o;
}

fn sh_ice(rd: vec3<f32>, tmax: f32, P: ShP) -> Sh {
    var o = sh0();
    let t = P.wt;
    let k = P.k;
    let frost = vec3<f32>(0.85, 0.96, 1.0);
    // The blizzard closing in.
    let rmax = mix(50.0, 16.0, smoothstep(0.0, 0.9, k));
    let wr = sh_weather(rd, tmax, P.base, P.time, P.seed, 1, smoothstep(0.0, 1.8, t), 3.0, rmax, -2.0, 40.0);
    o.l = o.l + frost * wr.x * 0.9;
    o.d = max(o.d, wr.y * 0.1);
    // Frost racing in over the ground.
    if (P.band > 0.0 && P.hd < 42.0) {
        let rf = 36.0 * (1.0 - smoothstep(0.05, 0.75, k));
        let cover = smoothstep(rf, rf + 3.0, P.hd) * (1.0 - smoothstep(36.0, 42.0, P.hd));
        let c = zcracks(P.hd, P.gang, 42.0, P.seed + 3.0);
        o.l = o.l + mix(P.col, frost, 0.5) * (c * 1.4 + 0.12 + ln(P.hd - rf, 0.6) * 0.8) * cover * P.band;
    }
    // Ice spires growing out of the ground and leaning in, a great crystal in their middle.
    for (var j = 0; j < 8; j = j + 1) {
        let fj = f32(j);
        var A = P.base;
        var Bt = P.base;
        var w = 2.4;
        var g = smoothstep(0.4, 0.85, k);
        if (j < 7) {
            let a = fj * 0.8976 + P.seed * 0.01;
            g = smoothstep(0.08 + 0.06 * fj, 0.42 + 0.06 * fj, k);
            A = P.base + vec3<f32>(cos(a) * 8.5, -1.0, sin(a) * 8.5);
            Bt = A + vec3<f32>(-cos(a) * 4.0, 17.0 + 4.0 * hash(fj + P.seed), -sin(a) * 4.0);
            w = 1.5;
        } else {
            Bt = P.base + vec3<f32>(0.0, 24.0, 0.0);
        }
        if (g <= 0.0) { continue; }
        let B = mix(A, Bt, g);
        let q = sh_seg(rd, tmax, A, B);
        let wt = w * pow(max(1.0 - q.y, 0.0), 0.8) + 0.05;
        let sd = q.x - wt;
        let px = sh_px(B);
        let body = smoothstep(0.12 * px, -0.12, sd) * q.z;
        let facet = 0.6 + 0.4 * step(0.5, fract(q.y * 4.0 + fj * 0.3)) + pow(hash(floor(q.y * 7.0) + fj), 6.0) * 1.2;
        o.l = o.l + mix(P.col, frost, 0.45) * (body * (0.25 + 0.4 * (1.0 - q.x / max(wt, 0.05))) * facet + ln(sd, 0.14 * px) * 1.5 * q.z);
        o.w = o.w + ln(sd, 0.05 * px) * 0.35 * q.z;
        o.d = max(o.d, body * 0.22);
    }
    o.l = o.l + sh_floor(P, frost);
    return o;
}

fn ft_time(i: i32) -> f32 {
    if (i == 0) { return 1.3; }
    if (i == 1) { return 2.7; }
    if (i == 2) { return 3.9; }
    return 4.9;
}

fn sh_fighting(rd: vec3<f32>, tmax: f32, P: ShP) -> Sh {
    var o = sh0();
    let t = P.wt;
    let k = P.k;
    let ki = mix(P.col, vec3<f32>(1.0, 0.75, 0.35), 0.35);
    let hotc = vec3<f32>(1.0, 0.95, 0.85);
    // A ki aura roaring up round the shrine, taller as it builds.
    let Hf = 6.0 + 22.0 * k;
    let qa = sh_vax(rd, tmax, P.base, Hf);
    let yf = qa.y / Hf;
    let n = noise1(qa.y * 0.8 - P.time * 9.0 + qa.w * 2.0 + P.seed);
    let rf = 3.6 * (1.0 - 0.6 * yf) * (1.0 + 0.25 * n);
    o.l = o.l + ki * (ln(qa.x - rf, 0.5) * 0.55 + exp(-sq(qa.x / rf)) * 0.08) * (1.0 - yf) * qa.z * smoothstep(0.0, 0.8, t);
    // Ki strikes slamming down onto it, each with a shock ring (fx.rs `fist_times`).
    for (var i = 0; i < 4; i = i + 1) {
        let dt = t - ft_time(i);
        if (dt < 0.0 || dt > 0.9) { continue; }
        let env = exp(-dt * 7.0);
        let qp = sh_vax(rd, tmax, P.base, 60.0);
        let w = (1.0 + 1.2 * exp(-dt * 12.0)) * sh_px(P.base);
        o.l = o.l + (ki * exp(-sq(qp.x / w)) * 0.9 + hotc * exp(-sq(qp.x / (w * 0.3))) * 0.7) * env * qp.z;
        if (P.band > 0.0) {
            let u = dt / 0.9;
            let rr = 2.0 + 26.0 * (1.0 - sq(1.0 - u));
            o.l = o.l + ki * ln(P.hd - rr, 0.5 + 0.6 * u) * pow(1.0 - u, 1.5) * 1.8 * P.band;
            o.d = max(o.d, ln(P.hd - rr * 0.85, 1.6) * 0.3 * (1.0 - u) * P.band);
        }
    }
    // A sphere of focused ki gathering over it, ringed by a spinning shock halo.
    let S = 4.5 * smoothstep(0.38, 0.72, k);
    if (S > 0.05) {
        let orb = sh_orb(rd, tmax, P.foc, S, ki, P.time, P.seed, 0.6);
        o.l = o.l + orb.l * 0.6;
        o.w = o.w + orb.w * 0.4;
        let px = sh_px(P.foc);
        let h = sh_plane(rd, tmax, P.base, P.foc.y - P.base.y);
        if (h.z > 0.0) {
            let d2 = h.xy - (P.foc.xz - P.base.xz);
            let r = length(d2);
            let ang = atan2(d2.y, d2.x);
            let rr = S * (1.9 + 0.08 * sin(ang * 6.0 - P.time * 14.0));
            o.l = o.l + ki * ln(r - rr, 0.35 * px + 0.2) * 0.9;
        }
    }
    o.l = o.l + sh_floor(P, ki);
    return o;
}

fn ps_time(i: i32) -> vec2<f32> {
    if (i == 0) { return vec2<f32>(1.6, 12.0); }
    if (i == 1) { return vec2<f32>(3.4, 22.0); }
    return vec2<f32>(4.8, 32.0);
}

fn sh_poison(rd: vec3<f32>, tmax: f32, P: ShP) -> Sh {
    var o = sh0();
    let t = P.wt;
    let k = P.k;
    let goo = mix(P.col, vec3<f32>(0.55, 1.0, 0.35), 0.25);
    let amt = smoothstep(0.0, 2.0, t);
    o = sh_mix(o, sh_cloud(rd, tmax, P, 38.0, 36.0, amt, goo));
    // Miasma rolling in over the ground and hanging low round the shrine.
    if (P.band > 0.0 && P.hd < 34.0) {
        let n = nz2(P.gp.xz * 0.12 + vec2<f32>(P.time * 0.3, -P.time * 0.2) + vec2<f32>(P.seed));
        let dens = smoothstep(-0.25, 0.6, n) * (1.0 - smoothstep(26.0, 34.0, P.hd)) * amt;
        o.d = max(o.d, dens * 0.42 * P.band);
        o.l = o.l + goo * dens * 0.25 * P.band;
    }
    let haze = sh_weather(rd, tmax, P.base, P.time, P.seed, 5, amt, 8.0, 24.0, -1.0, 9.0);
    o.l = o.l + goo * haze.x * 0.12;
    o.d = max(o.d, haze.x * 0.12);
    // Bubbles swelling and popping on the shrine.
    for (var i = 0; i < 9; i = i + 1) {
        let fi = f32(i);
        let u = fract(t / 1.3 + hash(fi * 1.7 + P.seed));
        let a = fi * 2.399;
        let rr = 3.0 * hash(fi * 3.1 + P.seed);
        let c = P.base + vec3<f32>(cos(a) * rr, 0.8 + u * 5.0, sin(a) * rr);
        let R = 0.4 + 1.3 * u;
        let hb = ray_ball(rd, tmax, c, R);
        let pop = 1.0 - smoothstep(0.88, 0.95, u);
        o.l = o.l + (goo * ln(hb.x - R, 0.12) * 1.2 + vec3<f32>(1.0) * sh_pt(rd, tmax, c + vec3<f32>(-0.3 * R, 0.4 * R, 0.0), 0.12 * R).x * 0.8) * hb.y * pop * amt;
    }
    // The toxic geyser: three eruptions, each higher.
    for (var i = 0; i < 3; i = i + 1) {
        let e = ps_time(i);
        let dt = t - e.x;
        if (dt < 0.0 || dt > 1.2) { continue; }
        let hg = e.y * smoothstep(0.0, 0.22, dt) * (1.0 - smoothstep(0.6, 1.2, dt));
        let q = sh_vax(rd, tmax, P.base, hg + 0.5);
        let rg = (2.4 + 0.7 * noise1(q.y * 0.9 - P.time * 8.0 + P.seed)) * (1.0 + 0.6 * smoothstep(hg * 0.8, hg, q.y)) * (1.0 + P.camd / 200.0);
        let body = exp(-sq(q.x / rg));
        o.l = o.l + (goo * (body * 0.9 + ln(q.x - rg, 0.4) * 0.8) + vec3<f32>(0.9, 1.0, 0.8) * ln(q.y - hg, 1.0) * body * 0.8) * q.z;
        o.d = max(o.d, body * 0.35 * q.z);
        let side = normalize(cross(vec3<f32>(0.0, 1.0, 0.0), rd) + vec3<f32>(1e-4, 0.0, 0.0));
        let sp = sh_sparks(q.x * 0.9, q.y * 0.5 + P.time * 4.0, 0.2, P.seed + f32(i)) * step(q.x, 6.0) * smoothstep(0.3, 0.6, dt);
        o.l = o.l + goo * sp * 1.2 * q.z;
    }
    o.l = o.l + sh_floor(P, goo);
    return o;
}

fn sh_ground(rd: vec3<f32>, tmax: f32, P: ShP) -> Sh {
    var o = sh0();
    let t = P.wt;
    let k = P.k;
    let magma = mix(P.col, vec3<f32>(1.0, 0.5, 0.15), 0.55);
    let earth = vec3<f32>(0.36, 0.27, 0.18);
    // Fissures tearing across the ground.
    if (P.band > 0.0) {
        let reach = 34.0 * smoothstep(0.02, 0.6, k);
        let c = zcracks(P.hd, P.gang, reach, P.seed) + 0.6 * zcracks(P.hd, P.gang + 0.3, reach * 0.7, P.seed + 9.0);
        o.l = o.l + magma * c * (1.4 + 0.5 * sin(P.time * 6.0 + P.hd)) * P.band;
        o.d = max(o.d, smoothstep(0.0, 0.3, c) * 0.4 * P.band);
    }
    // Dust kicked up and blown round.
    let dust = sh_weather(rd, tmax, P.base, P.time, P.seed, 5, smoothstep(0.3, 2.0, t), 6.0, 30.0, -1.0, 8.0);
    o.d = max(o.d, dust.x * 0.25);
    o.l = o.l + magma * dust.x * 0.05;
    // The earth pillar: it bursts up out of the shrine, rumbles, then comes crashing back down.
    let rise = smoothstep(0.42, 0.55, k) * (1.0 - smoothstep(0.86, 1.0, k));
    let H = 42.0 * rise;
    if (H > 0.3) {
        let q = sh_vax(rd, tmax, P.base + vec3<f32>(0.0, -1.0, 0.0), H + 1.0);
        let rp = (3.4 + 0.5 * noise1(q.y * 0.7 + q.w * 3.0 + P.seed)) * (1.0 - 0.3 * smoothstep(H * 0.7, H + 1.0, q.y));
        let sd = q.x - rp;
        let px = sh_px(P.base);
        let body = smoothstep(0.2 * px, -0.2, sd) * q.z;
        let seam = ln(fract(q.y * 0.16 + 0.3 * noise1(q.w * 3.0 + P.seed)) - 0.5, 0.05);
        o.d = max(o.d, body * 0.9);
        o.l = o.l + earth * body * 0.3 + magma * (seam * body * 1.3 + ln(sd, 0.3 * px) * 0.7 * q.z);
        let side = normalize(cross(vec3<f32>(0.0, 1.0, 0.0), rd) + vec3<f32>(1e-4, 0.0, 0.0));
        let sp = sh_sparks(q.x * 1.2, q.y * 0.6 - P.time * 6.0, 0.18, P.seed) * step(q.x, rp * 2.5) * step(rp, q.x);
        o.l = o.l + mix(magma, vec3<f32>(1.0, 0.9, 0.7), 0.4) * sp * q.z;
        for (var i = 0; i < 8; i = i + 1) {
            let fi = f32(i);
            let a = fi * 2.399 + t * 1.6;
            let c = P.base + vec3<f32>(cos(a) * 7.5, H * (0.2 + 0.7 * hash(fi + P.seed)), sin(a) * 7.5);
            o = sh_mix(o, sh_rock(rd, tmax, c, 0.8 + 0.6 * hash(fi * 3.0), magma, 0.8));
        }
    }
    o.l = o.l + sh_floor(P, magma);
    return o;
}

fn sh_flying(rd: vec3<f32>, tmax: f32, P: ShP) -> Sh {
    var o = sh0();
    let t = P.wt;
    let k = P.k;
    let air = mix(P.col, vec3<f32>(0.92, 0.96, 1.0), 0.45);
    o = sh_mix(o, sh_cloud(rd, tmax, P, 62.0, 50.0, smoothstep(0.0, 1.5, t), air));
    // Winds whipping round it.
    let wr = sh_weather(rd, tmax, P.base, P.time, P.seed, 2, smoothstep(0.2, 1.8, t), 5.0, 32.0, -1.0, 52.0);
    o.l = o.l + air * wr.x * 0.8;
    // The funnel reaching down out of the cloud.
    let yt = 62.0 * (1.0 - smoothstep(0.08, 0.92, k));
    let q = sh_vax(rd, tmax, P.base + vec3<f32>(0.0, yt, 0.0), 62.0 - yt);
    let s = q.y / max(62.0 - yt, 0.01);
    let rf = (1.2 + 10.0 * pow(s, 2.0)) * (1.0 + 0.1 * noise1(q.y * 0.5 - t * 7.0 + P.seed)) * (1.0 + P.camd / 200.0);
    let swirl = 0.5 + 0.5 * sin(4.0 * q.w + q.y * 0.5 - t * 12.0);
    let shell = ln(q.x - rf, 0.5 + 0.12 * rf) * (1.0 - 0.75 * smoothstep(0.6, 1.0, s));
    let fill = exp(-sq(q.x / rf));
    let on = smoothstep(0.03, 0.15, k);
    o.l = o.l + air * (shell * (0.7 + 0.7 * swirl) + fill * 0.3) * q.z * on;
    o.d = max(o.d, fill * 0.25 * q.z * on);
    // Feathers caught in it.
    for (var i = 0; i < 18; i = i + 1) {
        let fi = f32(i);
        let h1 = hash(fi * 1.3 + P.seed);
        let h2 = hash(fi * 2.7 + P.seed);
        let r = (6.0 + 14.0 * h1) * (1.0 - 0.6 * k);
        let a = fi * 2.399 + t * (1.5 + 1.5 * h2) * (1.0 + k);
        let y = 3.0 + 40.0 * h2;
        let p0 = P.base + vec3<f32>(cos(a) * r, y + sin(t * 2.0 + fi), sin(a) * r);
        let tg = vec3<f32>(-sin(a), 0.3 * sin(t * 3.0 + fi), cos(a));
        let qf = sh_seg(rd, tmax, p0 - tg * 0.5, p0 + tg * 0.5);
        o.l = o.l + vec3<f32>(1.0, 1.0, 1.0) * exp(-sq(qf.x / (0.12 * sh_px(p0)))) * 0.9 * qf.z * smoothstep(0.0, 1.0, t);
    }
    o.l = o.l + sh_floor(P, air);
    return o;
}

fn sh_bug(rd: vec3<f32>, tmax: f32, P: ShP) -> Sh {
    var o = sh0();
    let t = P.wt;
    let k = P.k;
    let glowc = mix(P.col, vec3<f32>(0.9, 1.0, 0.4), 0.35);
    let silk = vec3<f32>(0.95, 0.95, 0.8);
    // The swarm converging from all round, closer and denser.
    let rs = mix(38.0, 8.0, smoothstep(0.05, 0.8, k));
    let sw = sh_weather(rd, tmax, P.base, P.time, P.seed, 3, smoothstep(0.0, 1.2, t) * (1.0 - smoothstep(0.85, 0.95, k)), rs * 0.3, rs, -1.0, 26.0);
    o.d = max(o.d, min(sw.x * 0.45, 0.6));
    o.l = o.l + glowc * sw.x * 0.25;
    // Glowing leaders spiralling in.
    for (var i = 0; i < 24; i = i + 1) {
        let fi = f32(i);
        let h1 = hash(fi * 1.1 + P.seed);
        let h2 = hash(fi * 2.3 + P.seed);
        let e = smoothstep(0.05 + 0.3 * h2, 0.8, k);
        let r = mix(24.0 + 18.0 * h1, 3.5, e);
        let a = fi * 2.399 + t * (1.2 + h2) + 3.0 * e;
        let jit = vec3<f32>(noise1(P.time * 6.0 + fi), noise1(P.time * 5.0 + fi * 3.0), noise1(P.time * 7.0 + fi * 5.0)) * 0.8;
        let p = P.base + vec3<f32>(cos(a) * r, mix(2.0 + 22.0 * h2, 5.0, e), sin(a) * r) + jit;
        let g = sh_pt(rd, tmax, p, 0.2 * sh_px(p));
        o.l = o.l + glowc * (g.x * 1.6 + g.y * 0.15) * (0.6 + 0.4 * sin(P.time * 9.0 + fi)) * (1.0 - smoothstep(0.88, 0.96, k));
    }
    // The cocoon they spin over the shrine: silk, a heartbeat glowing inside, then cracks.
    let Rc = 4.6 * smoothstep(0.3, 0.68, k);
    if (Rc > 0.05) {
        let Lc = 13.0 * smoothstep(0.3, 0.68, k);
        let q = sh_vax(rd, tmax, P.base + vec3<f32>(0.0, 1.0, 0.0), Lc);
        let prof = pow(max(sin(3.14159 * q.y / max(Lc, 0.01)), 0.0), 0.6);
        let sd = q.x - Rc * prof;
        let px = sh_px(P.base);
        let body = smoothstep(0.15 * px, -0.15, sd) * q.z;
        let strands = pow(0.5 + 0.5 * sin(q.w * 6.0 + q.y * 1.8), 16.0) + pow(0.5 + 0.5 * sin(-q.w * 5.0 + q.y * 1.3 + 1.0), 16.0);
        let beat = 0.5 + 0.5 * sin(t * (3.0 + 9.0 * k));
        let crack = smoothstep(0.84, 0.97, k) * ln(sin(q.w * 4.0 + q.y * 0.9 + 2.0 * noise1(q.y * 0.7 + P.seed)), 0.12);
        o.d = max(o.d, body * 0.72);
        o.l = o.l + silk * strands * body * 0.5 + glowc * (body * beat * 0.4 * k + crack * body * 2.0 + ln(sd, 0.25 * px) * 0.7 * q.z);
        o.w = o.w + crack * body * 0.6;
    }
    o.l = o.l + sh_floor(P, glowc);
    return o;
}

fn rock_at(i: i32, P: ShP, s: f32) -> vec4<f32> {
    let fi = f32(i);
    let h1 = hash(fi * 1.7 + P.seed);
    let h2 = hash(fi * 2.3 + P.seed);
    let h3 = hash(fi * 3.9 + P.seed);
    let kk = s / P.fall;
    let ti = 0.03 + 0.25 * h1;
    let lift = smoothstep(ti, ti + 0.15, kk);
    let a = fi * 2.399 + s * (0.5 + 1.4 * kk);
    let r = (14.0 + 12.0 * h2) * (1.0 - 0.55 * smoothstep(0.5, 0.72, kk));
    let orbit = P.base + vec3<f32>(cos(a) * r, mix(-1.5, 10.0 + 8.0 * h3 + 4.0 * sin(a), lift), sin(a) * r);
    let C = P.base + vec3<f32>(0.0, 22.0, 0.0);
    // Fibonacci points on a sphere: where each boulder sits in the assembled one.
    let zz = 1.0 - 2.0 * (fi + 0.5) / 16.0;
    let rr = sqrt(max(1.0 - zz * zz, 0.0));
    let tgt = C + vec3<f32>(cos(fi * 2.399) * rr, zz, sin(fi * 2.399) * rr) * 4.2;
    let gath = smoothstep(0.66, 0.82, kk);
    let slam = vec3<f32>(0.0, -18.0, 0.0) * smoothstep(0.86, 1.0, kk);
    return vec4<f32>(mix(orbit, tgt, gath) + slam * gath, 1.2 + 1.2 * hash(fi * 5.1));
}

fn sh_rock_t(rd: vec3<f32>, tmax: f32, P: ShP) -> Sh {
    var o = sh0();
    let k = P.k;
    let rim = mix(P.col, vec3<f32>(1.0, 0.75, 0.4), 0.3);
    let gath = smoothstep(0.66, 0.82, k);
    // The heart they gather round: hot light in the gaps.
    let C = P.base + vec3<f32>(0.0, 22.0 - 18.0 * smoothstep(0.86, 1.0, k), 0.0);
    let orb = sh_orb(rd, tmax, C, 3.2 * gath, mix(rim, P.hot, 0.5), P.time, P.seed, 0.3);
    o.l = o.l + orb.l * 0.35;
    // Boulders torn out of the ground, orbiting, then crushing together.
    for (var i = 0; i < 16; i = i + 1) {
        let b = rock_at(i, P, P.wt);
        o = sh_mix(o, sh_rock(rd, tmax, b.xyz, b.w, rim, 1.0 + 1.5 * gath));
    }
    if (P.band > 0.0) {
        o.l = o.l + rim * zcracks(P.hd, P.gang, 26.0 * smoothstep(0.0, 0.3, k), P.seed) * 0.9 * P.band;
    }
    o.l = o.l + sh_floor(P, rim);
    return o;
}

fn st_time(i: i32) -> f32 {
    if (i == 0) { return 0.9; }
    if (i == 1) { return 1.6; }
    if (i == 2) { return 2.25; }
    if (i == 3) { return 2.85; }
    if (i == 4) { return 3.4; }
    return 3.9;
}

fn sh_steel(rd: vec3<f32>, tmax: f32, P: ShP) -> Sh {
    var o = sh0();
    let t = P.wt;
    let k = P.k;
    let metal = mix(P.col, vec3<f32>(0.9, 0.95, 1.0), 0.3);
    let spark = vec3<f32>(1.0, 0.8, 0.45);
    // Giant blades falling out of the sky and staking into the ground round the shrine (fx.rs `blade_times`).
    for (var i = 0; i < 6; i = i + 1) {
        let fi = f32(i);
        let e = clamp((t - (st_time(i) - 0.45)) / 0.45, 0.0, 1.0);
        if (t < st_time(i) - 0.45) { continue; }
        let a = fi * 1.0472 + P.seed * 0.01;
        let up = normalize(vec3<f32>(cos(a) * 0.35, 1.0, sin(a) * 0.35));
        let drop = vec3<f32>(0.0, 90.0 * (1.0 - e * e), 0.0);
        let tip = P.base + vec3<f32>(cos(a) * 12.0, -2.0, sin(a) * 12.0) + drop;
        let hilt = tip + up * 26.0;
        let q = sh_seg(rd, tmax, tip, hilt);
        let px = sh_px(hilt);
        let w = 1.8 * pow(q.y, 0.45) + 0.06;
        let sd = q.x - w;
        let body = smoothstep(0.12 * px, -0.12, sd) * q.z;
        let sheen = pow(0.5 + 0.5 * sin(q.y * 10.0 - t * 6.0 + fi), 20.0);
        let side = perp(up);
        let g = sh_seg(rd, tmax, hilt - side * 3.2, hilt + side * 3.2);
        let guard = smoothstep(0.1 * px, -0.1, g.x - 0.5) * g.z;
        o.d = max(o.d, max(body, guard) * 0.6);
        o.l = o.l + metal * (max(body, guard) * (0.18 + sheen * 1.2) + ln(sd, 0.12 * px) * 1.3 * q.z + ln(g.x - 0.5, 0.1 * px) * 0.9 * g.z);
        o.w = o.w + sheen * body * 0.3;
        // Sparks where it bites into the ground.
        let ds = t - st_time(i);
        if (ds >= 0.0 && ds < 0.6) {
            let sp = sh_pt(rd, tmax, tip - drop + up * 2.0, 2.5 * px);
            o.l = o.l + spark * (sp.x * 1.5 + sp.y * 0.3) * exp(-ds * 6.0);
        }
    }
    // The great gear lowering over them, spinning, locking into place.
    let gon = smoothstep(0.45, 0.55, k);
    if (gon > 0.0) {
        let yg = mix(60.0, 16.0, smoothstep(0.5, 0.95, k));
        let h = sh_plane(rd, tmax, P.base, yg);
        if (h.z > 0.0) {
            let r = length(h.xy);
            let a = atan2(h.y, h.x) - t * (0.5 + 2.0 * k);
            let tooth = smoothstep(0.42, 0.5, abs(fract(a / 6.2831853 * 16.0) - 0.5) * 2.0);
            let Ro = 16.0 + 2.4 * (1.0 - tooth);
            let Ri = 12.5;
            let ring = smoothstep(Ri - 0.1, Ri + 0.1, r) * (1.0 - smoothstep(Ro - 0.1, Ro + 0.1, r));
            let spoke = (1.0 - smoothstep(0.8, 1.2, abs(sin(3.0 * a)) * r)) * step(r, Ri) * step(2.0, r);
            let hub = 1.0 - smoothstep(3.0, 3.4, r);
            let solid = max(ring, max(spoke, hub)) * gon;
            let glint = pow(0.5 + 0.5 * cos(a - t * 2.5), 28.0);
            o.d = max(o.d, solid * 0.65);
            o.l = o.l + metal * (solid * (0.2 + glint * 1.4) + (ln(r - Ro, 0.15) + ln(r - Ri, 0.12)) * 0.9 * gon);
        }
    }
    o.l = o.l + sh_floor(P, metal);
    return o;
}

fn sh_arrive(ty: i32, rd: vec3<f32>, tmax: f32, P: ShP) -> Sh {
    if (ty == 0) { return sh_normal(rd, tmax, P); }
    if (ty == 2) { return sh_water(rd, tmax, P); }
    if (ty == 3) { return sh_electric(rd, tmax, P); }
    if (ty == 4) { return sh_grass(rd, tmax, P); }
    if (ty == 5) { return sh_ice(rd, tmax, P); }
    if (ty == 6) { return sh_fighting(rd, tmax, P); }
    if (ty == 7) { return sh_poison(rd, tmax, P); }
    if (ty == 8) { return sh_ground(rd, tmax, P); }
    if (ty == 9) { return sh_flying(rd, tmax, P); }
    if (ty == 10) { return sh_psychic(rd, tmax, P); }
    if (ty == 11) { return sh_bug(rd, tmax, P); }
    if (ty == 12) { return sh_rock_t(rd, tmax, P); }
    if (ty == 13) { return sh_ghost(rd, tmax, P); }
    if (ty == 14) { return sh_dragon(rd, tmax, P); }
    if (ty == 15) { return sh_dark(rd, tmax, P); }
    if (ty == 16) { return sh_steel(rd, tmax, P); }
    return sh_fairy(rd, tmax, P);
}

// The climax's type aftermath, ct seconds after it (on top of the Z release burst and the column lighting).
// A climax shell (`ray_ball`) that fades out as it grows towards the camera, so it never washes over the
// whole picture when it reaches the viewer.
fn sh_shell(rd: vec3<f32>, tmax: f32, c: vec3<f32>, r: f32) -> vec2<f32> {
    let hb = ray_ball(rd, tmax, c, r);
    let near = 1.0 - smoothstep(0.55, 0.85, r / max(length(c), 1.0));
    return vec2<f32>(hb.x, hb.y * near);
}

fn sh_climax(ty: i32, rd: vec3<f32>, tmax: f32, P: ShP, ct: f32) -> Sh {
    var o = sh0();
    if (ct < 0.0 || ct > 2.6) { return o; }
    let col = P.col;
    let fade = 1.0 - smoothstep(1.6, 2.6, ct);
    let white = vec3<f32>(1.0, 0.97, 0.94);
    // A ground wave every type shares, in its colour.
    if (P.band > 0.0) {
        let u = clamp(ct / 1.6, 0.0, 1.0);
        let rr = 2.0 + 30.0 * (1.0 - pow(1.0 - u, 2.4));
        o.l = o.l + col * ln(P.hd - rr, 0.4 + 0.9 * u) * pow(1.0 - u, 1.6) * 1.4 * P.band;
    }
    if (ty == 2) {
        // The whirlpool crashes down: a dome of spray and a tidal ring racing out.
        let u = clamp(ct / 1.8, 0.0, 1.0);
        let rw = 3.0 + 28.0 * (1.0 - pow(1.0 - u, 2.0));
        let hw = 7.0 * (1.0 - u) * smoothstep(0.0, 0.15, ct);
        let tc = sh_cyl(rd, P.base, rw);
        for (var e = 0; e < 2; e = e + 1) {
            let tt = select(tc.y, tc.x, e == 0);
            if (tt > 0.0 && tt < tmax) {
                let p = rd * tt - P.base;
                let inw = smoothstep(-1.0, 0.0, p.y) * (1.0 - smoothstep(hw - 0.8, hw, p.y));
                let a = atan2(p.z, p.x);
                let crest = ln(p.y - hw + 0.4 + 0.4 * noise1(a * 9.0 + ct * 4.0), 0.5);
                o.l = o.l + (col * inw * 0.45 + white * crest * 0.9) * fade;
                o.d = max(o.d, inw * 0.3 * fade);
            }
        }
        let dome = 12.0 * (1.0 - pow(1.0 - clamp(ct / 0.9, 0.0, 1.0), 2.0));
        let hb = sh_shell(rd, tmax, P.base, dome);
        o.l = o.l + mix(col, white, 0.5) * ln(hb.x - dome, 0.8) * (1.0 - smoothstep(0.3, 1.2, ct)) * hb.y;
    } else if (ty == 3) {
        // One huge discharge: a plasma ball blowing out, arcs racing over the ground.
        let r = 14.0 * (1.0 - pow(1.0 - clamp(ct / 0.6, 0.0, 1.0), 3.0));
        let hb = sh_shell(rd, tmax, P.base + vec3<f32>(0.0, 1.0, 0.0), r);
        let env = 1.0 - smoothstep(0.2, 1.1, ct);
        o.l = o.l + col * (ln(hb.x - r, 0.6 + r * 0.05) * 1.6 + smoothstep(r, 0.0, hb.x) * 0.25) * env * hb.y;
        for (var j = 0; j < 6; j = j + 1) {
            let a = f32(j) * 1.0472 + P.seed;
            let end = P.base + vec3<f32>(cos(a), 0.0, sin(a)) * (6.0 + 22.0 * clamp(ct / 0.5, 0.0, 1.0));
            let b = sh_bolt(rd, tmax, P.base + vec3<f32>(0.0, 1.5, 0.0), end, f32(j) * 5.0 + floor(ct * 12.0), sh_px(P.base));
            o.l = o.l + (white * b.x * 1.6 + col * b.y * 0.4) * (1.0 - smoothstep(0.3, 0.8, ct));
        }
    } else if (ty == 15) {
        // The eclipse bursts: a black shell blowing out, its rim burning, the light coming back.
        let r = 26.0 * (1.0 - pow(1.0 - clamp(ct / 1.3, 0.0, 1.0), 2.5));
        let hb = sh_shell(rd, tmax, P.base + vec3<f32>(0.0, 2.0, 0.0), r);
        let env = 1.0 - smoothstep(0.6, 2.0, ct);
        o.d = max(o.d, smoothstep(r, r * 0.8, hb.x) * 0.55 * env * hb.y + 0.42 * (1.0 - smoothstep(0.0, 1.4, ct)) / (1.0 + sq(P.camd / 110.0)));
        o.l = o.l + mix(col, vec3<f32>(0.95, 0.15, 0.3), 0.5) * ln(hb.x - r, 0.5 + r * 0.04) * 1.8 * env * hb.y;
    } else if (ty == 17 || ty == 4) {
        // A flower of light blooming out over the ground.
        if (P.band > 0.0) {
            let u = clamp(ct / 1.4, 0.0, 1.0);
            let R = 26.0 * (1.0 - pow(1.0 - u, 2.0));
            let a = P.gang + ct * 0.4;
            let pr = R * (0.45 + 0.55 * abs(cos(2.5 * a)));
            o.l = o.l + mix(col, white, 0.35) * (ln(P.hd - pr, 0.6) * 1.4 + (1.0 - smoothstep(pr * 0.7, pr, P.hd)) * 0.2) * fade * P.band;
        }
    } else if (ty == 11) {
        // Wings of light unfurling out of the cocoon.
        let c = P.base + vec3<f32>(0.0, 6.0, 0.0);
        let uv = sh_bb(rd, c);
        let open = smoothstep(0.0, 0.6, ct) * fade;
        let x = abs(uv.x) / (14.0 * open + 0.01);
        let y = uv.y / 10.0;
        let wing = smoothstep(1.0, 0.85, length(vec2<f32>(x - 0.5, y - 0.25) * vec2<f32>(1.0, 1.3))) + smoothstep(0.7, 0.55, length(vec2<f32>(x - 0.4, y + 0.45) * vec2<f32>(1.0, 1.4)));
        let vein = pow(0.5 + 0.5 * sin(atan2(y, x) * 9.0), 12.0);
        let vis = select(0.0, 1.0, dot(c, rd) < tmax);
        o.l = o.l + mix(col, white, 0.3) * clamp(wing, 0.0, 1.0) * (0.35 + vein * 0.8) * open * vis;
    } else if (ty == 9) {
        // A gust ring blasting out low over the ground.
        let r = 3.0 + 34.0 * (1.0 - pow(1.0 - clamp(ct / 1.0, 0.0, 1.0), 2.0));
        let h = sh_plane(rd, tmax, P.base, 2.5);
        if (h.z > 0.0) {
            // Fades where it passes close under the camera (it filled half the picture there).
            let th = abs((P.base.y + 2.5) / min(rd.y, -1e-4));
            let near = smoothstep(8.0, 22.0, th);
            o.l = o.l + mix(col, white, 0.6) * ln(length(h.xy) - r, 1.0) * (1.0 - smoothstep(0.4, 1.2, ct)) * 1.2 * near;
        }
    } else if (ty == 5 || ty == 12 || ty == 16 || ty == 8) {
        // Shards, rubble or sparks flung out.
        let c = P.base + vec3<f32>(0.0, 2.0, 0.0);
        let qr = sh_shell(rd, tmax, c, 30.0);
        let uv = sh_bb(rd, c);
        let rr = length(uv);
        let a = atan2(uv.y, uv.x);
        let sp = sh_sparks(a * 9.0, rr * 0.5 - ct * 14.0, 0.18, P.seed) * smoothstep(1.0, 4.0, rr) * (1.0 - smoothstep(16.0 * ct + 4.0, 16.0 * ct + 10.0, rr));
        var sc = mix(col, white, 0.6);
        if (ty == 16) { sc = vec3<f32>(1.0, 0.78, 0.4); }
        o.l = o.l + sc * sp * 1.6 * (1.0 - smoothstep(0.6, 1.6, ct)) * select(0.0, 1.0, dot(c, rd) < tmax) * qr.y;
    } else if (ty == 13) {
        // The gate's ring of runes stamped out over the ground.
        if (P.band > 0.0) {
            let u = clamp(ct / 1.2, 0.0, 1.0);
            let R = 24.0 * (1.0 - pow(1.0 - u, 2.0));
            let cell = P.gang / 6.2831853 * 28.0;
            let rn = rune(fract(cell), clamp((P.hd - R + 1.2) / 2.4, 0.0, 1.0), hash(floor(cell) + P.seed)) * step(abs(P.hd - R), 1.2);
            o.l = o.l + col * (rn * 1.6 + ln(P.hd - R - 1.4, 0.2)) * fade * P.band;
        }
    } else if (ty == 10) {
        // Reality snapping back: ripples racing out through the air.
        for (var j = 0; j < 3; j = j + 1) {
            let cj = ct - f32(j) * 0.18;
            if (cj > 0.0) {
                let r = 4.0 + 30.0 * (1.0 - pow(1.0 - clamp(cj / 1.2, 0.0, 1.0), 2.0));
                let hb = sh_shell(rd, tmax, P.base + vec3<f32>(0.0, 3.0, 0.0), r);
                o.l = o.l + col * ln(hb.x - r, 0.4 + r * 0.02) * (1.0 - smoothstep(0.3, 1.2, cj)) * 1.2 * hb.y;
            }
        }
    } else if (ty == 7) {
        // The last eruption: the geyser at full height, raining back down.
        let hg = 40.0 * smoothstep(0.0, 0.25, ct) * (1.0 - smoothstep(1.0, 2.2, ct));
        let q = sh_vax(rd, tmax, P.base, hg + 0.5);
        let rg = (2.6 + 0.8 * noise1(q.y * 0.7 - P.time * 8.0)) * (1.0 + P.camd / 200.0);
        let body = exp(-sq(q.x / rg));
        o.l = o.l + col * (body * 1.0 + ln(q.x - rg, 0.6) * 0.8) * q.z;
        o.d = max(o.d, body * 0.3 * q.z);
    } else if (ty == 0 || ty == 6 || ty == 14) {
        // A shell of force bursting out.
        let r = 22.0 * (1.0 - pow(1.0 - clamp(ct / 0.9, 0.0, 1.0), 2.5));
        let hb = sh_shell(rd, tmax, P.base + vec3<f32>(0.0, 1.5, 0.0), r);
        var sc = mix(col, white, 0.4);
        if (ty == 14) { sc = mix(col, vec3<f32>(1.0, 0.55, 0.9), 0.4); }
        o.l = o.l + sc * ln(hb.x - r, 0.4 + r * 0.04) * (1.0 - smoothstep(0.3, 1.1, ct)) * 1.6 * hb.y;
    }
    return o;
}

fn shrinefx(f: Fx, rd: vec3<f32>, tmax: f32, time: f32) -> vec4<f32> {
    let tau = 6.2831853;
    let base = f.a.xyz;
    let fl = i32(floor(f.p.z / 32.0 + 0.001));
    let ty = i32(f.p.z + 0.5) - fl * 32;
    let seed = f.p.y;
    let col = f.color.rgb;
    let hot = mix(col, vec3<f32>(1.0, 0.92, 0.75), 0.4);
    let flash_ok = 1.0 - clamp(u.info.z, 0.0, 1.0);
    let fall = max(f.m.w, 0.1);
    let wt = f.m.y;
    let prog = clamp(f.m.x, 0.0, 1.0);
    let camd = length(base.xz);
    var light = vec3<f32>(0.0);
    var white = 0.0;
    var dark = 0.0;
    // The floor round the shrine, for the warning, the capture ring and the crown's wave.
    var hd = 1e9;
    var band = 0.0;
    var gang = 0.0;
    var gpv = vec3<f32>(0.0);
    if (tmax < 9000.0) {
        let gp = rd * tmax - base;
        gpv = gp;
        hd = length(gp.xz);
        band = smoothstep(-2.5, -1.2, gp.y) * (1.0 - smoothstep(0.9, 1.6, gp.y));
        gang = atan2(gp.z, gp.x);
    }

    let P = ShP(base, f.b.xyz, col, hot, gpv, wt, clamp(wt / fall, 0.0, 1.0), fall, time, seed, band, hd, gang, camd);
    // ---- 1. Every type but fire: its own arrival (sh_arrive), the focus in b.
    if (fl == 0 && ty != 1) {
        let o = sh_arrive(ty, rd, tmax, P);
        light = light + o.l;
        white = white + o.w;
        dark = max(dark, o.d);
    }

    // ---- 1. Fire: a huge meteor, slow at first, then a slam.
    if (fl == 0 && ty == 1) {
        let k = clamp(wt / fall, 0.0, 1.0);
        let appear = smoothstep(0.0, 0.7, wt);
        let mpos = f.b.xyz;
        let tip = base + vec3<f32>(0.0, 1.0, 0.0);
        let tv = tip - mpos;
        let tl = length(tv);
        var dir = vec3<f32>(0.0, -1.0, 0.0);
        if (tl > 0.01) { dir = tv / tl; }
        let kd = sh_kind(ty);
        let R = 5.0 * (1.0 + length(mpos) / 160.0);
        // The trail, stretching as it speeds up.
        let L = 16.0 + 50.0 * k;
        let tb = mpos - dir * L;
        let qt = ray_seg(rd, 100000.0, mpos, tb);
        let axp = mpos + (tb - mpos) * qt.w;
        let offt = qt.xyz - axp;
        let dt = length(offt);
        let occt = 1.0 - smoothstep(tmax - 1.5, tmax + 1.5, length(qt.xyz));
        let w0 = R * (1.0 - 0.78 * qt.w) + 0.4;
        if (dt < w0 * 4.0) {
            let xs = qt.w * L;
            let fv = zflav(ty, xs, atan2(offt.z, offt.x), time, seed);
            let fade = pow(1.0 - qt.w, 1.4) * occt * appear;
            light = light + col * (exp(-sq(dt / (w0 * 0.55 * fv.x))) * 1.4 * fv.y + exp(-sq(dt / (w0 * 1.6))) * 0.4) * fade;
            white = white + exp(-sq(dt / (w0 * 0.22))) * fv.w * pow(1.0 - qt.w, 3.0) * occt * appear;
            dark = max(dark, exp(-sq(dt / (w0 * 1.7))) * kd.z * 0.4 * smoothstep(0.05, 0.35, qt.w) * (1.0 - qt.w) * occt * appear);
            // Embers and shards peeling off it, streaming back along the trail.
            let side = normalize(cross(dir, rd) + vec3<f32>(1e-4, 0.0, 0.0));
            let sx = dot(offt, side);
            let sp = sh_sparks(sx * 1.6, xs * 0.55 + time * 7.0, 0.16, seed) * step(abs(sx), w0 * 2.2);
            light = light + mix(hot, vec3<f32>(1.0), 0.35) * sp * 1.6 * fade;
            // Electric: arcs crawling round the trail.
            if (kd.w > 0.0) {
                let arcs = pow(0.5 + 0.5 * sin(atan2(offt.z, offt.x) * 4.0 + noise1(xs * 1.7 - time * 24.0 + seed) * 5.0), 24.0);
                light = light + vec3<f32>(0.9, 0.95, 1.0) * arcs * exp(-sq((dt - w0 * 0.9) / 0.35)) * 2.0 * fade * kd.w;
            }
        }
        // The body.
        let tc = dot(mpos, rd);
        if (tc > -R) {
            let hb = ray_ball(rd, tmax, mpos, R);
            let h = hb.x;
            let disc = smoothstep(R, R * 0.93, h) * hb.y * appear;
            let nz = sqrt(max(1.0 - sq(h / R), 0.0));
            let q = rd * max(tc, 0.0) - mpos;
            let ax = normalize(cross(rd, vec3<f32>(0.0, 1.0, 0.0)) + vec3<f32>(1e-4, 0.0, 0.0));
            let ay = cross(ax, rd);
            let uv = vec2<f32>(dot(q, ax), dot(q, ay)) / R;
            let uvr = rot2(uv, time * 0.5 + seed);
            let n = normalize(uv.x * ax + uv.y * ay - rd * nz);
            let heat = pow(clamp(dot(n, dir), 0.0, 1.0), 1.5);
            let n1 = noise1(uvr.x * 3.3 + seed) + 0.7 * noise1(uvr.y * 2.9 + seed * 1.3) + 0.35 * noise1((uvr.x - uvr.y) * 7.0 + seed);
            let cracks = exp(-sq(n1 * 3.5)) * nz;
            // Rock: dims what is behind it, glows along its cracks and on its burning front.
            dark = max(dark, disc * kd.x * 0.82);
            light = light + hot * (cracks * kd.y * 2.0 + heat * heat * 1.6 * kd.x + kd.x * 0.08) * disc;
            // Energy: a bright orb with bands swirling in it.
            let orb = 1.0 - kd.x;
            let swirl = 0.5 + 0.5 * sin(atan2(uvr.y, uvr.x) * 3.0 + length(uv) * 9.0 - time * 5.0 + seed);
            light = light + col * (0.7 + 0.8 * nz + 0.5 * swirl) * disc * orb;
            white = white + pow(nz, 6.0) * 0.6 * disc * orb + heat * 0.25 * disc;
            // Rim and a wide soft halo (Lorentzian), so it reads from across the map.
            let behind = clamp((tmax - (tc - R)) / R, 0.0, 1.0);
            light = light + col * (ln(h - R, R * 0.16) * 1.3 + 0.55 / (1.0 + sq(h / (2.4 * R)))) * behind * appear;
        }
        // On the shrine: a warning ring closing in, its shadow, the floor heating.
        if (band > 0.0 && hd < 24.0) {
            let rr = mix(18.0, 3.0, k * k);
            let pulse = 0.65 + 0.35 * sin(time * (4.0 + 9.0 * k));
            light = light + col * ln(hd - rr, 0.25 + 0.25 * k) * (0.6 + 1.0 * k) * pulse * band * appear;
            let tick = ln(abs(fract(gang / tau * 16.0 + time * 0.2) - 0.5) * rr * 0.4, 0.05) * ln(hd - rr - 0.9, 0.35);
            light = light + col * tick * 0.8 * band * appear;
            dark = max(dark, (1.0 - smoothstep(rr * 0.25, rr * 0.95, hd)) * 0.5 * k * k * band);
            light = light + hot * exp(-sq(hd / (1.5 + 2.5 * k))) * k * k * 1.5 * band;
        }
        // A faint guide thread from it down to the shrine.
        let qd = ray_seg(rd, tmax, mpos, tip);
        let dd = length(qd.xyz - (mpos + tv * qd.w));
        light = light + col * exp(-sq(dd / 0.12)) * 0.35 * appear * (0.6 + 0.4 * noise1(time * 6.0 + qd.w * 20.0));
    }

    // ---- 2. Awake: the column it left, a ring that shows where to stand, and the capture.
    if (fl >= 1 && fl <= 3) {
        let ign = smoothstep(fall, fall + 0.9, wt);
        let war = select(0.0, 1.0, fl == 3);
        let H = 96.0;
        let p0 = base + vec3<f32>(0.0, 0.2, 0.0);
        let p1 = base + vec3<f32>(0.0, H, 0.0);
        let q = ray_seg(rd, 100000.0, p0, p1);
        let x = q.w * H;
        let off = q.xyz - (p0 + (p1 - p0) * q.w);
        let d = length(off);
        let occ = smoothstep(tmax - 0.6, tmax + 0.6, length(q.xyz));
        // Faint behind terrain, but not past its foot (a ray under it would draw a streak down the ground).
        let vis = mix(1.0, 0.22 * step(0.002, q.w), occ);
        let surge = 0.5 + 0.5 * noise1(time * 0.8 + seed);
        var r = (0.5 + 0.3 * prog) * (1.0 + 0.12 * noise1(x * 0.2 - time * 1.6 + seed) + 0.18 * surge);
        r = r * (1.0 + 1.6 * exp(-x / 3.0)) * (1.0 + camd / 90.0);
        let top = 1.0 - smoothstep(H * 0.4, H, x);
        // Contested: the column stutters (shallow, never a strobe) and fights red.
        let stut = 1.0 - war * 0.3 * step(0.55, hash(floor(time * 7.0) + seed));
        let redk = war * (0.5 + 0.3 * sin(time * 8.0));
        let bc = mix(mix(col, sh_gold(), prog * 0.85), sh_red(), redk);
        let lvl = ign * top * vis * stut * (1.0 + 1.2 * prog);
        // A fresh column flares as it lights.
        let lit = 1.0 + 2.0 * exp(-max(wt - fall, 0.0) * 1.5) * ign;
        let coreI = exp(-sq(d / (0.45 * r)));
        // The type's character in the column (zflav: water twists, electric surges, dark drinks the
        // light); fire keeps the plain column it was approved with.
        var zf = vec4<f32>(1.0, 1.0, 0.0, 0.45);
        if (ty != 1) { zf = zflav(ty, x, atan2(off.z, off.x), time * 0.6, seed); }
        let sheath = exp(-sq(d / (1.8 * r * zf.x)));
        let halo = 1.0 / (1.0 + sq(d / (6.0 * r)));
        light = light + bc * (sheath * 0.9 * (0.55 + 0.45 * zf.y) + halo * 0.25 + coreI * 0.8 * prog) * lvl * lit;
        dark = max(dark, sheath * zf.z * 0.45 * ign * top * vis);
        white = white + coreI * 0.55 * ign * top * vis * stut * lit;
        // Strands twisting up it: wide and lazy when free, pulled in tight and fast as the count fills.
        let ang = atan2(off.z, off.x);
        let spin = 1.4 + 7.0 * prog;
        let rs = r * mix(2.4, 1.05, prog);
        let wband = exp(-sq((d - rs) / (0.35 * r + 0.12)));
        let s1 = pow(0.5 + 0.5 * sin(3.0 * ang + 0.35 * x - time * spin + seed), 10.0) * wband;
        light = light + mix(col, sh_gold(), prog) * s1 * 1.5 * lvl;
        if (war > 0.0) {
            let s2 = pow(0.5 + 0.5 * sin(3.0 * ang - 0.35 * x + time * spin * 1.4 + seed * 2.0), 10.0) * wband;
            light = light + sh_red() * s2 * 1.6 * ign * top * vis;
        }
        // Motes rising up it, faster while captured.
        let side = normalize(cross(vec3<f32>(0.0, 1.0, 0.0), rd) + vec3<f32>(1e-4, 0.0, 0.0));
        let sx = dot(off, side);
        let sp = sh_sparks(sx * 2.0, x * 0.6 - time * (1.6 + 5.0 * prog), 0.12 + 0.1 * prog, seed) * step(abs(sx), r * 4.0) * (1.0 - smoothstep(16.0, 50.0, x));
        light = light + mix(bc, vec3<f32>(1.0), 0.45) * sp * 1.5 * ign * vis;
        // The capture ring: a gold halo climbing the column with the count.
        if (prog > 0.001 && war < 0.5 && abs(rd.y) > 1e-4) {
            let hy = base.y + 1.2 + prog * 44.0;
            let tp = hy / rd.y;
            if (tp > 0.0 && tp < tmax) {
                let hp = rd * tp - base;
                let rr = length(hp.xz);
                let rad = 2.0 + 0.8 * prog + 0.15 * sin(time * 3.0);
                light = light + sh_gold() * (ln(rr - rad, 0.16) * 1.8 + ln(rr - rad, 0.6) * 0.3) * ign;
                white = white + ln(rr - rad, 0.05) * 0.7 * ign;
            }
        }
        // The floor: the capture ring (where you must stand), filling gold round as the count runs.
        if (band > 0.0 && hd < f.a.w + 5.0) {
            let zr = f.a.w;
            let ringc = mix(col, sh_red(), war);
            let jag = war * 0.3 * noise1(gang * 7.0 + time * 9.0 + seed);
            light = light + ringc * (ln(hd - zr - jag, 0.16) * 1.3 + ln(hd - zr, 0.5) * 0.25) * (0.8 + 0.25 * sin(time * 2.0)) * band * ign;
            let filled = step(fract(gang / tau + 0.25), prog) * (1.0 - war);
            light = light + sh_gold() * ln(hd - zr, 0.22) * filled * 1.8 * band * ign;
            white = white + ln(hd - zr, 0.06) * filled * 0.6 * band * ign;
            let ticks = ln(abs(fract(gang / tau * 12.0) - 0.5) * hd, 0.06) * smoothstep(zr - 1.0, zr - 0.5, hd) * (1.0 - smoothstep(zr - 0.2, zr, hd));
            light = light + ringc * ticks * 0.9 * band * ign;
            light = light + bc * exp(-sq(hd / 3.2)) * 0.45 * (1.0 + prog) * band * ign;
        }
    }

    // ---- 2b. The climax's type aftermath, over the Z release burst (fire has the burst alone).
    if (fl >= 1 && fl <= 3 && ty != 1 && wt - fall < 2.6) {
        let oc = sh_climax(ty, rd, tmax, P, wt - fall);
        light = light + oc.l;
        white = white + oc.w;
        dark = max(dark, oc.d);
    }

    // ---- 3. The crown: the column surges gold and launches up, a golden shell bursts out, a crown of light.
    if (fl == 4) {
        let tt = clamp(f.p.x, 0.0, 1.0) * f.p.w;
        let gold = sh_gold();
        let p0 = base + vec3<f32>(0.0, 0.2, 0.0);
        let p1 = base + vec3<f32>(0.0, 20.0 + tt * 140.0, 0.0);
        let q = ray_seg(rd, 100000.0, p0, p1);
        let d = length(q.xyz - (p0 + (p1 - p0) * q.w));
        let vis = mix(1.0, 0.22 * step(0.002, q.w), smoothstep(tmax - 0.6, tmax + 0.6, length(q.xyz)));
        let env = smoothstep(0.0, 0.08, tt) * exp(-tt * 1.0);
        let w = (0.7 + 1.6 * exp(-sq((tt - 0.15) / 0.22))) * (1.0 + camd / 90.0);
        light = light + mix(gold, col, 0.25) * (exp(-sq(d / w)) * 1.3 + 0.35 / (1.0 + sq(d / (5.0 * w)))) * env * vis;
        white = white + exp(-sq(d / (0.3 * w))) * env * 0.6 * vis;
        let k2 = clamp(tt / 1.5, 0.0, 1.0);
        if (k2 > 0.0 && k2 < 1.0) {
            let rs = 26.0 * (1.0 - pow(1.0 - k2, 3.0));
            let hs = ray_ball(rd, tmax, base + vec3<f32>(0.0, 1.5, 0.0), rs);
            light = light + gold * ln(hs.x - rs, max(rs * 0.035, 0.08)) * (1.0 - k2) * hs.y;
        }
        // A crown of light hovering over the shrine.
        let ce = smoothstep(0.15, 0.5, tt) * (1.0 - smoothstep(2.3, f.p.w, tt));
        if (ce > 0.0 && abs(rd.y) > 1e-4) {
            let hy = base.y + 7.0 + tt * 0.6;
            let tp = hy / rd.y;
            if (tp > 0.0 && tp < tmax) {
                let hp = rd * tp - base;
                let rr = length(hp.xz);
                let a = atan2(hp.z, hp.x) + tt * 0.8;
                let spikes = rr - (2.6 + 1.1 * pow(0.5 + 0.5 * cos(5.0 * a), 8.0));
                light = light + gold * (ln(spikes, 0.12) * 1.8 + ln(rr - 2.6, 0.1) * 1.2 + exp(-sq(rr / 2.0)) * 0.25) * ce;
                white = white + ln(spikes, 0.04) * 0.8 * ce;
            }
        }
        // A wave across the ground and motes spiralling up.
        if (band > 0.0) {
            let kr = clamp(tt / 1.8, 0.0, 1.0);
            let rr = 1.0 + 30.0 * (1.0 - pow(1.0 - kr, 2.5));
            light = light + gold * ln(hd - rr, 0.3 + 0.6 * kr) * sq(1.0 - kr) * 1.6 * band;
        }
        let side = normalize(cross(vec3<f32>(0.0, 1.0, 0.0), rd) + vec3<f32>(1e-4, 0.0, 0.0));
        let qa = ray_seg(rd, tmax, p0, base + vec3<f32>(0.0, 30.0, 0.0));
        let offa = qa.xyz - (p0 + vec3<f32>(0.0, 29.8, 0.0) * qa.w);
        let sx = dot(offa, side);
        let sp = sh_sparks(sx * 1.4, qa.w * 15.0 - tt * 9.0, 0.22, seed) * step(abs(sx), 6.0) * step(0.002, qa.w);
        light = light + mix(gold, vec3<f32>(1.0), 0.5) * sp * 1.8 * smoothstep(0.1, 0.3, tt) * (1.0 - smoothstep(2.0, f.p.w, tt));
        // One soft pulse as it crowns (photosensitivity budget: single, at most 0.12, off with /capifx flash off).
        white = white + flash_ok * exp(-sq((tt - 0.06) / 0.07)) * 0.12 / (1.0 + length(base) / 24.0);
    }

    let peak = max(light.r, max(light.g, light.b));
    light = light * min(1.0, 3.0 / max(peak, 1e-4));
    let s = f.color.a;
    return vec4<f32>((light + vec3<f32>(min(white, 2.5))) * s, clamp(dark * s, 0.0, 0.85));
}

fn one(f: Fx, rd: vec3<f32>, tmax: f32, time: f32) -> vec4<f32> {
    // 64 on top of the shape: seen by its own player (`own_view`), no solid body.
    let sw = i32(f.b.w + 0.5);
    let own = sw >= 64;
    let shape = sw % 64;
    // Z-Moves have their own bounds (a sky pillar, a whole-picture flash).
    if (shape == 12) { return zgather(f, rd, tmax, time); }
    if (shape == 13) { return zrelease(f, rd, tmax, time); }
    if (shape == 14) { return capfx(f, rd, tmax, time); }
    if (shape == 15) { return ballfx(f, rd, tmax, time); }
    if (shape == 16) { return ropefx(f, rd, tmax, time); }
    if (shape == 17) { return arcfx(f, rd, tmax, time); }
    if (shape == 18) { return markfx(f, rd, tmax, time); }
    if (shape == 19) { return legend(f, rd, tmax, time); }
    if (shape == 20) { return shrinefx(f, rd, tmax, time); }
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
    // How side-on a cone is seen (0 along its axis, where its caster looks through it; 1 across).
    var cone_side = 1.0;
    // How far this pixel's part of the effect is from the camera (blocks), for the solid body:
    // a ball's surface, a line's nearest point to the view ray.
    var near_d = length(f.a.xyz) - r;
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
        if (shape == 6) { cone_side = 1.0 - abs(dot(rd, dir)); }
        near_d = length(q.xyz);
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
            w = r * 0.4;
            glowk = 0.3;
            corek = 0.9;
        } else if (ty == 10) {
            // Psychic: rings that pulse along it.
            w = r * 0.8;
            mask = 0.3 + 1.1 * exp(-sq((fract(x * 1.2 - time * 2.0) - 0.5) * 10.0));
        } else if (ty == 11) {
            // Bug: a buzzing swarm of specks.
            let cell = floor(x * 5.0);
            mask = step(0.5, hash(cell * 1.7 + seed + floor(time * 8.0)));
            on = on + (u1 * (hash(cell) - 0.5) + u2 * (hash(cell + 3.1) - 0.5)) * r * 0.9;
            w = r * 0.4;
            glowk = 0.35;
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
            // (0.25 / +0.2 read as a faint haze from the side: ground, rock, bug and ice cones
            // were barely there at 15 blocks.)
            core = vv * 0.35 * max(corek, 0.7) * mask * env;
            glow = vv * (max(glowk, 0.3) + 0.3) * mask * env;
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
        // A small burst round a body (a self buff or heal) is mostly rim seen edge-on: give it a
        // glowing fill so it reads from across a field.
        fillk = max(fillk, 0.3 * (1.0 - smoothstep(1.2, 2.5, r)));
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
        // Holds near full for the first part, then fades (1 - age was half gone in a blink).
        let env = 1.0 - age * age;
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
                // (0.14 balls were specks from 15 blocks.)
                let sb = ray_ball(rd, tmax, sp, 0.2);
                core = core + exp(-sq(sb.x / 0.1)) * sb.y * 1.4;
                glow = glow + exp(-sb.x / 0.2) * sb.y * 0.5;
            }
        } else if (fl == 2) {
            // Slow: blue rings tightening round the feet on the ground, and a cold tint on the body.
            glow = step(hb.x, r) * 0.2 * hb.y;
            dark = step(hb.x, r) * 0.08 * hb.y;
            if (tmax < 9000.0) {
                let p = rd * tmax;
                let dy = p.y - (f.a.y - r * 0.8);
                if (abs(dy) < 0.6) {
                    let dd = length((p - f.a.xyz).xz);
                    let k = fract(time * 0.8);
                    glow = glow + (ln(dd - r * (1.2 - 0.6 * k), 0.1) * (1.0 - k) + ln(dd - r * 0.55, 0.07) * 0.6) * 1.4;
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
            glow = rim * (0.45 + 1.4 * pow(hash(floor(s01 * 31.0) + floor(time * 25.0) + seed), 6.0)) * hb.y;
        } else if (fl == 7) {
            // A Potion: a green ring climbing the body, plus signs drifting up round it, and a
            // soft green wash that fades.
            let g = 1.0 - smoothstep(0.65, 1.0, age);
            let ringy = (age * 2.4 - 1.0) * r;
            let ring = ln(hb.x - r * 0.95, 0.09) * exp(-sq((v.y - ringy) / (r * 0.35)));
            glow = (step(hb.x, r) * 0.12 * (1.0 - age) + ring * 1.3) * g * hb.y;
            for (var i = 0; i < 7; i = i + 1) {
                let fi = f32(i);
                let aa = hash(fi * 3.1 + seed) * 6.2832 + time * 0.6;
                let rise = fract(age * 1.3 + hash(fi * 7.7));
                let pp = f.a.xyz + vec3<f32>(cos(aa) * r * 0.8, (rise * 2.2 - 0.9) * r, sin(aa) * r * 0.8);
                let tp = dot(pp, rd);
                if (tp > 0.0 && tp < tmax) {
                    let w2 = rd * tp - pp;
                    let px = abs(dot(w2, side));
                    let py = abs(w2.y);
                    let arm = 0.13;
                    let plus = max(step(px, arm) * ln(py, 0.028), step(py, arm) * ln(px, 0.028));
                    let fade = smoothstep(0.0, 0.15, rise) * (1.0 - smoothstep(0.7, 1.0, rise)) * g;
                    core = core + plus * fade * 1.5;
                    glow = glow + exp(-sq(length(w2) / 0.2)) * fade * 0.5;
                }
            }
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
    var light = (tinted + vec3<f32>(1.0) * white + f.m.rgb * mk2) * s;
    // A solid body under the glow (docs/fx/MOVE_FX_VISIBILITY.md): purely added light vanished on
    // bright sky, sand and snow (it can only push toward white), so a move's core and inner glow
    // also hide what's behind them (alpha) and its colour sits on top. The alpha reaches a little
    // further out than the light, which leaves a thin darker rim: pale types (ice, steel, flying,
    // fairy, normal) read on the sky by that outline. Not for the guard bubble, ground warnings,
    // zones or a dodge (strength 0.3): those stay see-through.
    var alpha = clamp(dark * s, 0.0, 0.85);
    if (shape != 4 && shape != 9 && shape != 10 && s > 0.3 && !own) {
        // Faded in by strength, so a status aura running out loses its body smoothly. Close to the
        // camera (the start of your own beam or cone, anything right in front of the lens) it
        // stays the old added light: solid there it covered the screen. Your own body-bound
        // effects and hits on you skip the body altogether (`own`), however far back the camera.
        let sk = min(s, 1.0) * smoothstep(0.3, 0.5, s) * smoothstep(3.0, 8.0, near_d);
        var amax = 0.8;
        // A cone is seen through by its own caster (from behind, along its axis), a status aura
        // sits on a body for seconds: kept half see-through.
        if (shape == 6) { amax = mix(0.35, 0.8, cone_side); }
        if (shape == 11) { amax = 0.55; }
        // A big burst (Earthquake, Heat Wave) spans a fight: its rim goes solid, its inside stays
        // mostly clear so the players in it can be seen.
        if (shape == 2) { amax = mix(0.8, 0.3, smoothstep(1.2, 2.5, r)); }
        let body = clamp(core * 1.3 + glow * 1.6 + white * 0.6 + mot * 0.8, 0.0, 1.0);
        // Where the added light is already bright (a hot core) it stays see-through: added to a
        // bright sky it clips to white, which is the most a pale core can stand out there.
        let lp = max(light.r, max(light.g, light.b));
        let solid = (1.0 - (1.0 - body) * (1.0 - body)) * amax * sk * (1.0 - 0.75 * smoothstep(0.45, 1.0, lp));
        // The colour that covers it: the type's own at full strength (hue kept), whiter at the
        // core, on top of (most of) the added light. Only replacing the light lost contrast: a
        // pale cover over mid-grey is barely different from it, while added it still brightens.
        let cn = cs / max(max(cs.r, max(cs.g, cs.b)), 1e-3);
        let cover = mix(cn, vec3<f32>(1.0), clamp(core, 0.0, 1.0) * 0.45 + clamp(white, 0.0, 1.0) * 0.5);
        light = light * (1.0 - 0.4 * solid) + cover * solid;
        // The outline: a thin band just outside the body, the type colour darkened, that hides
        // a little of what's behind. It's what lets white and pale moves read on a bright sky.
        // Pale colours (white, ice, steel, flying, fairy) and blues (water, dragon, ghost), which
        // melt into a daylight sky, get a stronger one.
        let pale = 1.0 - (max(cn.r, max(cn.g, cn.b)) - min(cn.r, min(cn.g, cn.b)));
        let blue = smoothstep(0.1, 0.4, cn.b - max(cn.r, cn.g) * 0.7);
        let rim = smoothstep(0.03, 0.12, body) * (1.0 - smoothstep(0.2, 0.45, body)) * mix(0.45, 0.8, max(pale, blue)) * sk * amax;
        light = light + cn * 0.18 * rim;
        alpha = 1.0 - (1.0 - alpha) * (1.0 - solid) * (1.0 - rim);
    }
    return vec4<f32>(light, alpha);
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
    // Light adds; the alpha dims what is behind: ghost and dark effects, and every move's solid
    // body and outline (docs/fx/MOVE_FX_VISIBILITY.md).
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
        if !self.off.load(Ordering::Relaxed)
            && FX.lock().map(|f| !f.low && (!f.effects.is_empty() || !f.guarding.is_empty() || f.capsule.is_some() || !f.shrines.is_empty())).unwrap_or(false)
            || !self.off.load(Ordering::Relaxed) && !low() && !crate::play::cosmetics::draw::legends().is_empty()
        {
            event.needs.world_depth = true;
        }
    }

    fn world_drawn(&self, event: &mut WorldDrawnEvent<'_, '_>) {
        if self.off.load(Ordering::Relaxed) {
            return;
        }
        let w = &mut *event.world;
        let eye = eye_offset(w.view_proj);
        let list = snapshot(w.camera + eye, view_forward(w.view_proj, eye));
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
        let no_flash = FX.lock().map(|f| f.no_flash).unwrap_or(false);
        floats.extend_from_slice(&[count as f32, EPOCH.elapsed().as_secs_f32() % 1000.0, if no_flash { 1.0 } else { 0.0 }, 0.0]);
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
    fn own_body_effects_and_hits_on_you_skip_the_solid_body() {
        let at = DVec3::new(0.0, 64.0, 0.0);
        let e = |shape: Shape, caster: &str, b: DVec3| Effect {
            shape,
            a: at,
            b,
            radius: 1.0,
            rgb: Vec3::ONE,
            seed: 0,
            start: Instant::now(),
            secs: 1.0,
            caster: caster.to_string(),
            strength: 1.0,
            style: 0,
            flags: 0,
            look: [0.0; 4],
        };
        let me = Some("onii");
        for shape in [Shape::Coat, Shape::Charge, Shape::Aura, Shape::Burst, Shape::Dash, Shape::Hit] {
            assert!(own_view(&e(shape, "onii", at), me, 1.8), "own {shape:?}");
            assert!(!own_view(&e(shape, "rival", at), me, 1.8), "someone else's {shape:?}");
            assert!(!own_view(&e(shape, "onii", at), None, 1.8), "{shape:?} before the name is known");
        }
        // Beams and cones reach out where you aim; strikes, zones and Z-Moves land elsewhere.
        for shape in [Shape::Beam, Shape::Cone, Shape::Ground, Shape::Zone, Shape::ZGather, Shape::ZRelease] {
            assert!(!own_view(&e(shape, "onii", at), me, 1.8), "{shape:?}");
        }
        assert!(!own_view(&e(Shape::Hit, "onii~fs1", at), me, 1.8), "a delayed strike lands elsewhere");
        assert!(!own_view(&e(Shape::Dash, "onii", at - DVec3::Y * 14.0), me, 1.8), "a meteor's comet");
        // Your own Earthquake keeps its solid rim (it shows the reach); a Wailord's self-buff puff
        // (radius 0.6 x 8) is still on its body.
        let quake = Effect { radius: 4.5, ..e(Shape::Burst, "onii", at) };
        assert!(!own_view(&quake, me, 1.8), "own wide burst");
        let puff = Effect { radius: 4.8, ..e(Shape::Burst, "onii", at) };
        assert!(own_view(&puff, me, 8.0), "a big species' self-buff");
        assert!(!own_view(&puff, me, 1.8), "the same size on a small species is an area move");
        // A hit on you, as the server sends it (`hit <who> ...`), and its packed shape number.
        heard(&["hit", "onii", "1,2,3", "fire", "12", "rival"], Some("onii"));
        heard(&["hit", "rival", "9,2,3", "fire", "12", "onii"], Some("onii"));
        let fx = FX.lock().unwrap();
        let on_me = fx.effects.iter().rev().find(|x| x.shape == Shape::Hit && x.caster == "onii").unwrap().clone();
        let on_them = fx.effects.iter().rev().find(|x| x.shape == Shape::Hit && x.caster == "rival").unwrap().clone();
        drop(fx);
        let shape_no = |x: &Effect| {
            let mut out = Vec::new();
            pack(x, Vec3::ZERO, Vec3::ZERO, 0.5, own_view(x, me, 1.8), &mut out);
            out[7]
        };
        assert_eq!(shape_no(&on_me), Shape::Hit as u32 as f32 + OWN_VIEW);
        assert_eq!(shape_no(&on_them), Shape::Hit as u32 as f32);
        // The shader takes the mark off with `% 64`: no real shape may reach it.
        assert!((Shape::ZRelease as u32 as f32) < OWN_VIEW);
        assert!(SHADER.contains("let own = sw >= 64;") && SHADER.contains("let shape = sw % 64;"));
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
    fn zmove_then_its_fx_gets_the_cinematic() {
        // Build-up on `zmove`, and the plain wind-up ring is left out for it.
        assert!(heard(&["zmove", "zed1", "hydropump", "water", "0.40"], None));
        assert!(!heard(&["charge", "zed1", "hydropump", "water", "0.40", "-"], None));
        {
            let fx = FX.lock().unwrap();
            assert!(fx.effects.iter().any(|e| e.caster == "zed1" && e.shape == Shape::ZGather && e.style == 2));
            assert!(!fx.effects.iter().any(|e| e.caster == "zed1" && e.shape == Shape::Charge), "a Z wind-up has no plain ring");
        }
        // The release: a beam from a to b lands its pillar at b; the build-up is over.
        assert!(heard(&["fx", "hydropump", "water", "beam", "0,1,0", "0,1,9", "0.6", "zed1", "8", "0.45"], None));
        let fx = FX.lock().unwrap();
        let z = fx.effects.iter().find(|e| e.caster == "zed1" && e.shape == Shape::ZRelease).expect("release");
        assert_eq!((z.style, z.flags), (2, 1), "water, beam form");
        assert_eq!(z.b, DVec3::new(0.0, 1.0, 9.0));
        assert!(fx.effects.iter().any(|e| e.caster == "zed1" && e.shape == Shape::Beam), "the move's own look still draws");
        assert!(!fx.effects.iter().any(|e| e.caster == "zed1" && e.shape == Shape::ZGather));
        assert!(!fx.zarmed.contains_key("zed1"));
        assert!(fx.later.iter().any(|(s, at, ..)| *s == "minecraft:entity.generic.explode" && *at == z.b));
    }

    #[test]
    fn a_healing_zmove_rises_instead_of_slamming() {
        assert!(heard(&["zmove", "zedH", "healpulse", "psychic", "2.00", "heal"], None));
        {
            let fx = FX.lock().unwrap();
            assert!(fx.effects.iter().any(|e| e.caster == "zedH" && e.shape == Shape::ZGather && e.flags == Z_HEAL_GATHER));
        }
        // support::cast shows it as a burst round the caster.
        assert!(heard(&["fx", "healpulse", "psychic", "burst", "5,64,5", "5,64,5", "10.0", "zedH", "7", "0.5"], None));
        let mut fx = FX.lock().unwrap();
        let z = fx.effects.iter().find(|e| e.caster == "zedH" && e.shape == Shape::ZRelease).expect("release");
        assert_eq!((z.flags, z.radius, z.a, z.b), (Z_HEAL_FORM, 10.0, DVec3::new(5.0, 64.0, 5.0), DVec3::new(5.0, 64.0, 5.0)));
        // Chimes, no explosion or thunder.
        let booms: Vec<&str> = fx.later.iter().filter(|l| l.1 == z.b).map(|l| l.0).collect();
        assert!(booms.contains(&"minecraft:block.amethyst_block.chime"), "{booms:?}");
        assert!(!booms.iter().any(|s| s.contains("explode") || s.contains("thunder")), "{booms:?}");
        fx.later.retain(|l| l.1 != DVec3::new(5.0, 64.0, 5.0));
        // The same line without `heal` (an older server) is the usual release.
        drop(fx);
        assert!(heard(&["zmove", "zedI", "healpulse", "psychic", "2.00"], None));
        assert!(heard(&["fx", "healpulse", "psychic", "burst", "9,64,9", "9,64,9", "10.0", "zedI", "7", "0.5"], None));
        let mut fx = FX.lock().unwrap();
        assert_eq!(fx.effects.iter().find(|e| e.caster == "zedI" && e.shape == Shape::ZRelease).map(|e| e.flags), Some(2));
        fx.later.retain(|l| l.1 != DVec3::new(9.0, 64.0, 9.0));
    }

    #[test]
    fn the_same_move_without_zmove_is_plain() {
        // A generated Z-Move has the same id as a move in slots 1-4: only `zmove` tells them apart.
        assert!(heard(&["fx", "hydropump", "water", "beam", "0,1,0", "0,1,9", "0.6", "zed2", "8", "0.45"], None));
        // An armed caster's other move isn't the Z-Move either.
        assert!(heard(&["zmove", "zed2", "blizzard", "ice", "0"], None));
        assert!(heard(&["fx", "icebeam", "ice", "beam", "0,1,0", "0,1,9", "0.6", "zed2", "8", "0.45"], None));
        let fx = FX.lock().unwrap();
        assert!(!fx.effects.iter().any(|e| e.caster == "zed2" && e.shape == Shape::ZRelease));
        assert!(fx.zarmed.contains_key("zed2"), "still waiting for its own fx");
    }

    #[test]
    fn interrupting_a_zmove_drops_its_build_up() {
        assert!(heard(&["zmove", "zed3", "flareblitz", "fire", "0.80"], None));
        assert!(!heard(&["charge", "zed3", "-"], None));
        assert!(heard(&["fx", "flareblitz", "fire", "dash", "0,1,0", "4,1,0", "0.5", "zed3", "4", "0.3"], None));
        let fx = FX.lock().unwrap();
        assert!(!fx.effects.iter().any(|e| e.caster == "zed3" && matches!(e.shape, Shape::ZGather | Shape::ZRelease)));
    }

    #[test]
    fn field_and_ground_zmoves_land_on_their_point() {
        // Tempest is a field: the server sends only its zone (server/sig.rs `release`).
        assert!(heard(&["zmove", "zed4", "sig:f_tempest", "flying", "0.00"], None));
        assert!(heard(&["fx", "sig:f_tempest", "flying", "zone", "3,1,3", "3,1,3", "14.00", "zed4", "4", "6.00"], None));
        assert!(heard(&["zmove", "zed5", "earthquake", "ground", "0.3"], None));
        assert!(heard(&["fx", "earthquake", "ground", "ground", "6,0,6", "6,0,6", "4", "zed5", "4", "0.6"], None));
        let fx = FX.lock().unwrap();
        let t = fx.effects.iter().find(|e| e.caster == "zed4" && e.shape == Shape::ZRelease).expect("tempest");
        assert_eq!((t.flags, t.a, t.b, t.radius), (5, DVec3::new(3.0, 1.0, 3.0), DVec3::new(3.0, 1.0, 3.0), 14.0));
        // It fired at once: its build-up still shows (for `Z_GATHER_MIN`) beside the release.
        assert!(fx.effects.iter().any(|e| e.caster == "zed4" && e.shape == Shape::ZGather));
        let q = fx.effects.iter().find(|e| e.caster == "zed5" && e.shape == Shape::ZRelease).expect("quake");
        assert_eq!((q.flags, q.b), (5, DVec3::new(6.0, 0.0, 6.0)));
    }

    #[test]
    fn a_delayed_zmove_releases_when_it_lands() {
        // Future Sight as a Z-Move fires at once, warns under `<name>~futuresight` and lands
        // 2 s later under that tag (server/play.rs `fire`).
        let back = |fx: &mut Fx, s: f32| {
            let arm = fx.zarmed.get_mut("zedF").expect("still armed");
            arm.at = arm.at.checked_sub(std::time::Duration::from_secs_f32(s)).unwrap();
        };
        assert!(heard(&["zmove", "zedF", "futuresight", "psychic", "0.00"], None));
        // An earlier slots 1-4 Future Sight still in flight lands before the Z's warning: plain.
        assert!(heard(&["fx", "futuresight", "psychic", "ground", "9,0,9", "9,0,9", "2.50", "zedF~futuresight", "2", "0.6"], None));
        assert!(!heard(&["charge", "zedF~futuresight", "futuresight", "psychic", "2.00", "circle", "5,0,5", "5,0,5", "2.50"], None));
        {
            let fx = FX.lock().unwrap();
            assert!(fx.effects.iter().any(|e| e.caster == "zedF~futuresight" && e.shape == Shape::Warn), "the landing warning still shows");
        }
        // ...or lands after it, somewhere else: plain too.
        assert!(heard(&["fx", "futuresight", "psychic", "ground", "9,0,9", "9,0,9", "2.50", "zedF~futuresight", "2", "0.6"], None));
        {
            let mut fx = FX.lock().unwrap();
            assert!(!fx.effects.iter().any(|e| e.shape == Shape::ZRelease && e.caster.starts_with("zedF")), "an earlier Future Sight isn't the Z");
            // 2.1 s later: past the wind-up's grace, but the delay keeps it armed.
            back(&mut fx, 2.1);
        }
        // Meanwhile: interrupting a new wind-up doesn't call it back, and the same move cast
        // from slots 1-4 is plain (its wind-up ring shows, its own fx doesn't release).
        assert!(!heard(&["charge", "zedF", "-"], None));
        assert!(!heard(&["charge", "zedF", "futuresight", "psychic", "0.30", "-"], None));
        {
            let fx = FX.lock().unwrap();
            assert!(fx.effects.iter().any(|e| e.caster == "zedF" && e.shape == Shape::Charge), "a slot cast's ring isn't hidden");
        }
        assert!(heard(&["fx", "futuresight", "psychic", "ground", "1,0,1", "1,0,1", "2.50", "zedF", "3", "0.6"], None));
        {
            let fx = FX.lock().unwrap();
            assert!(!fx.effects.iter().any(|e| e.shape == Shape::ZRelease && e.caster.starts_with("zedF")), "a plain fx after it left isn't the Z");
        }
        assert!(heard(&["fx", "futuresight", "psychic", "ground", "5,0,5", "5,0,5", "2.50", "zedF~futuresight", "3", "0.6"], None));
        let fx = FX.lock().unwrap();
        let z = fx.effects.iter().find(|e| e.shape == Shape::ZRelease && e.caster == "zedF~futuresight").expect("release");
        assert_eq!((z.style, z.flags, z.b), (type_index("psychic"), 5, DVec3::new(5.0, 0.0, 5.0)));
        assert!(!fx.zarmed.contains_key("zedF"));
    }

    #[test]
    fn zmove_shake_ramps_jolts_and_fades_with_distance() {
        assert!(z_shake(true, 1.0, 2.0) > z_shake(true, 0.0, 2.0), "the build-up ramps");
        let jolt = z_shake(false, 0.15 / Z_RELEASE_SECS, 2.0);
        assert!(jolt > 0.6, "{jolt}");
        assert!(z_shake(false, 0.9, 2.0) < 0.25 * jolt, "then settles");
        assert!(z_shake(false, 0.15 / Z_RELEASE_SECS, 20.0) < jolt);
        assert_eq!(z_shake(false, 0.15 / Z_RELEASE_SECS, 60.0), 0.0, "too far to feel");
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

    #[test]
    fn the_capsule_falls_back_to_the_hud_without_the_pass() {
        assert!(!capsule_flat_for(false, false), "the pass draws it");
        assert!(capsule_flat_for(true, false), "Low FX draws no pass");
        assert!(capsule_flat_for(false, true), "a pass that switched itself off draws nothing either");
    }

    /// Review: it was placed from the player's eye, not the over-the-shoulder camera players see
    /// through. The effects pass now places it from the camera it draws with.
    #[test]
    fn the_capsule_floats_ahead_of_the_camera_it_is_drawn_with() {
        // `frame`'s place, from somewhere else entirely (the eye, last frame).
        let cap = CapsuleFx { tier: 1, rgb: Vec3::ONE, start: Instant::now(), ahead: 1.0, at: Some(DVec3::new(500.0, 70.0, 500.0)), yaw: 0.0, played: 0, seed: 3, trail: None };
        let look = DVec3::new(0.3, -0.2, 0.9).normalize();
        // A shoulder camera and a far-off one: the same place relative to each.
        let (lens, other) = (DVec3::new(10.0, 65.0, -4.0), DVec3::new(-200.0, 120.0, 37.5));
        let a = capsule_drawn(&cap, lens, Some(look)).unwrap().a - lens;
        let b = capsule_drawn(&cap, other, Some(look)).unwrap().a - other;
        assert!((a - b).length() < 1e-9, "{a:?} vs {b:?}");
        // On the view axis, CAPSULE_AHEAD out, CAPSULE_DROP below it.
        let along = a + DVec3::Y * CAPSULE_DROP;
        assert!((along.length() - CAPSULE_AHEAD).abs() < 1e-6, "{along:?}");
        assert!(along.normalize().dot(look) > 0.9999, "on the view axis: {along:?}");
        // Looking steeply down: the same compass direction, pitch held at 25 degrees.
        let steep = DVec3::new(0.0, -0.95, 0.31).normalize();
        let s = capsule_drawn(&cap, lens, Some(steep)).unwrap().a - lens + DVec3::Y * CAPSULE_DROP;
        assert!(s.z > 0.0 && s.x.abs() < 1e-9, "{s:?}");
        assert!(((-s.y / s.length()).asin().to_degrees() - 25.0).abs() < 1e-6, "{s:?}");
        // Straight down has no compass direction: the yaw `frame` saw (90: facing -x).
        let down = CapsuleFx { yaw: 90.0, ..cap.clone() };
        let d = capsule_drawn(&down, lens, Some(DVec3::NEG_Y)).unwrap().a - lens;
        assert!(d.x < -1.0 && d.z.abs() < 1e-6, "{d:?}");
        // No view direction: where `frame` put it.
        assert_eq!(capsule_drawn(&cap, lens, None).unwrap().a, cap.at.unwrap());
    }

    /// The view direction read back from the effects pass's camera-relative matrix, with the
    /// game's reversed depth and with view bobbing moving the eye off the origin.
    #[test]
    fn the_view_direction_comes_out_of_the_matrix() {
        let fwd = Vec3::new(-0.4, -0.3, 0.6).normalize();
        let eye = Vec3::new(0.05, -0.03, 0.02);
        for proj in [Mat4::perspective_rh(1.2, 1.6, 0.05, 300.0), Mat4::perspective_rh(1.2, 1.6, 300.0, 0.05)] {
            let vp = proj * Mat4::look_to_rh(eye, fwd, Vec3::Y);
            let got = view_forward(vp, eye.as_dvec3()).expect("a direction");
            assert!(got.dot(fwd.as_dvec3()) > 0.99999, "{got:?} vs {fwd:?}");
        }
        assert_eq!(view_forward(Mat4::ZERO, DVec3::ZERO), None, "a degenerate matrix gives none");
    }

    /// The whole effects shader through naga, wgpu's own WGSL front end and validator: a slip in
    /// any effect (the capsule's included) fails here instead of switching the pass off in game.
    #[test]
    fn the_effects_shader_parses_and_validates() {
        use wgpu::naga;
        let module = naga::front::wgsl::parse_str(SHADER).unwrap_or_else(|e| panic!("{}", e.emit_to_string(SHADER)));
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::default())
            .validate(&module)
            .unwrap_or_else(|e| panic!("{}", e.emit_to_string(SHADER)));
        for name in ["vs", "fs"] {
            assert!(module.entry_points.iter().any(|e| e.name == name), "entry point {name}");
        }
        assert!(module.functions.iter().any(|(_, f)| f.name.as_deref() == Some("capfx")), "the capsule's shader is in");
    }

    /// Renders the capsule opening's stages for each rarity on a real GPU (the game's own shader,
    /// pipeline and uniform layout) as PPMs in CAPSULE_PREVIEW_DIR, for the contact sheet
    /// (native/tools/capsule_sheet.py). `cargo test ... -- --ignored capsule_preview`.
    #[test]
    #[ignore = "needs a GPU; writes PPMs to CAPSULE_PREVIEW_DIR"]
    fn capsule_preview() {
        fn block<F: std::future::Future>(f: F) -> F::Output {
            let mut f = std::pin::pin!(f);
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            loop {
                if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                    return v;
                }
                std::thread::yield_now();
            }
        }
        let dir = std::path::PathBuf::from(std::env::var("CAPSULE_PREVIEW_DIR").expect("set CAPSULE_PREVIEW_DIR"));
        std::fs::create_dir_all(&dir).unwrap();
        // Rows of 256-byte multiples for the read-back.
        const W: u32 = 384;
        const H: u32 = 288;
        let pitch_deg: f32 = std::env::var("CAPSULE_PREVIEW_PITCH").ok().and_then(|s| s.parse().ok()).unwrap_or(8.0);
        let night = std::env::var("CAPSULE_PREVIEW_NIGHT").is_ok();
        // CAPSULE_PREVIEW_BODY=<model height>: the over-the-shoulder camera of a Pokémon that tall
        // (play.rs `shoulder`), with a stand-in body where the camera sees it and the ground at its feet.
        let body_h: Option<f32> = std::env::var("CAPSULE_PREVIEW_BODY").ok().and_then(|s| s.parse().ok());
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = block(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .expect("no GPU adapter");
        let (device, queue) = block(adapter.request_device(&wgpu::DeviceDescriptor::default())).expect("no device");
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let g = build(&device, format).expect("the game's pipeline");

        // The camera as native/tools/vfx_preview.py has it: eye at the origin, looking along +z,
        // pitched down; wgpu clip depth 0..1, 0 in the depth texture is sky.
        let (eye_y, near, far) = (1.62f32, 0.05f32, 300.0f32);
        let p = pitch_deg.to_radians();
        let fwd = Vec3::new(0.0, -p.sin(), p.cos());
        let vp = Mat4::perspective_rh(70f32.to_radians(), W as f32 / H as f32, near, far) * Mat4::look_to_rh(Vec3::ZERO, fwd, Vec3::Y);
        let inv = vp.inverse();
        // The body (centre, radii) as an ellipsoid, and the ground's height, camera-relative.
        let (body, ground) = match body_h {
            Some(h) => {
                let h = h.clamp(0.25, 9.0);
                let (back, side, lift) = ((1.0 + h * 1.6).clamp(1.4, 10.0), (0.35 + h * 0.45).clamp(0.5, 3.2), 0.1 + h * 0.1);
                // The lens is pivot + right * side + lift, then back along the view; right is -x here.
                let pivot = fwd * back + Vec3::new(side, -lift, 0.0);
                let feet = pivot - Vec3::Y * (h * 0.85);
                (Some((feet + Vec3::Y * (h * 0.5), Vec3::new(h * 0.33, h * 0.5, h * 0.33))), feet.y)
            }
            None => (None, -eye_y),
        };
        let mut depth16 = Vec::with_capacity((W * H) as usize);
        let mut bg = Vec::with_capacity((W * H * 4) as usize);
        for j in 0..H {
            for i in 0..W {
                let ndc = glam::Vec4::new((i as f32 + 0.5) / W as f32 * 2.0 - 1.0, 1.0 - (j as f32 + 0.5) / H as f32 * 2.0, 0.5, 1.0);
                let q = inv * ndc;
                let rd = (q.truncate() / q.w).normalize();
                let t = if rd.y < -1e-4 { ground / rd.y } else { f32::INFINITY };
                // The stand-in body, nearer than the ground where the ray meets it.
                let hit = body.and_then(|(c, r)| {
                    let (o, d) = (-c / r, rd / r);
                    let (a, b, cc) = (d.dot(d), o.dot(d), o.dot(o) - 1.0);
                    let disc = b * b - a * cc;
                    (disc >= 0.0).then(|| (-b - disc.sqrt()) / a).filter(|t| *t > 0.0)
                });
                if let Some(tb) = hit.filter(|tb| *tb < t) {
                    let pt = rd * tb;
                    let c = vp * pt.extend(1.0);
                    depth16.push(((c.z / c.w).clamp(0.0, 1.0) * 65535.0).round() as u16);
                    let shade = (0.55 + 0.45 * (-rd.x * 0.6 + 0.8)).clamp(0.3, 1.0);
                    let v = |x: f32| (x * shade * if night { 0.4 } else { 1.0 }) as u8;
                    bg.extend_from_slice(&[v(236.0), v(214.0), v(226.0), 255]);
                } else if t <= 200.0 {
                    let pt = rd * t;
                    let c = vp * pt.extend(1.0);
                    depth16.push(((c.z / c.w).clamp(0.0, 1.0) * 65535.0).round() as u16);
                    let odd = (pt.x.floor() as i64 + pt.z.floor() as i64) & 1 == 1;
                    let v: u8 = if night { if odd { 34 } else { 28 } } else if odd { 70 } else { 58 };
                    bg.extend_from_slice(&[v, v + if night { 6 } else { 18 }, v - 8, 255]);
                } else {
                    depth16.push(0);
                    bg.extend_from_slice(if night { &[16, 20, 38, 255] } else { &[110, 150, 200, 255] });
                }
            }
        }
        let size = wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 };
        let depth = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("preview depth"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth16Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let dbytes: Vec<u8> = depth16.iter().flat_map(|d| d.to_le_bytes()).collect();
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &depth, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &dbytes,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(W * 2), rows_per_image: Some(H) },
            size,
        );
        let color = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("preview color"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let read = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("preview read"),
            size: u64::from(W * H * 4),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("preview"),
            layout: &g.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: g.uniforms.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&depth_view) },
            ],
        });
        // Placed the way the effects pass places it in game: from the camera it draws with.
        let look = view_forward(vp, DVec3::ZERO).expect("a view direction");
        let times: Vec<f32> = std::env::var("CAPSULE_PREVIEW_TIMES")
            .ok()
            .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
            .unwrap_or_else(|| vec![0.2, 1.0, 1.9, 2.5, 2.64, 2.85, 3.3, 4.2, 6.0]);
        let colors = [0xB8C4D0u32, 0x4FA8FF, 0xFFC23F];
        let mut lines = Vec::new();
        for tier in 0..3u32 {
            for (k, &t) in times.iter().enumerate() {
                let c = |s: u32| ((colors[tier as usize] >> s) & 0xFF) as f32 / 255.0;
                let cap = CapsuleFx { tier, rgb: Vec3::new(c(16), c(8), c(0)), start: Instant::now(), ahead: t, at: Some(DVec3::ZERO), yaw: 0.0, played: 0, seed: 17, trail: None };
                let e = capsule_drawn(&cap, DVec3::ZERO, Some(look)).expect("placed");
                let a = e.a.as_vec3();
                let mut floats = Vec::with_capacity(UNIFORM_FLOATS);
                floats.extend_from_slice(&inv.to_cols_array());
                floats.extend_from_slice(&[W as f32, H as f32, W as f32, H as f32]);
                floats.extend_from_slice(&[1.0, t, 0.0, 0.0]);
                floats.extend_from_slice(&[0.0; 4]);
                pack(&e, a, a, t / CAPSULE_SECS, false, &mut floats);
                floats.resize(UNIFORM_FLOATS, 0.0);
                let bytes: Vec<u8> = floats.iter().flat_map(|f| f.to_le_bytes()).collect();
                queue.write_buffer(&g.uniforms, 0, &bytes);
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo { texture: &color, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                    &bg,
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(W * 4), rows_per_image: Some(H) },
                    size,
                );
                let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                {
                    let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("preview"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &color_view,
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
                enc.copy_texture_to_buffer(
                    wgpu::TexelCopyTextureInfo { texture: &color, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                    wgpu::TexelCopyBufferInfo {
                        buffer: &read,
                        layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(W * 4), rows_per_image: Some(H) },
                    },
                    size,
                );
                queue.submit([enc.finish()]);
                read.slice(..).map_async(wgpu::MapMode::Read, |r| r.expect("map"));
                device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
                let px = read.slice(..).get_mapped_range().expect("mapped").to_vec();
                read.unmap();
                let mut ppm = format!("P6\n{W} {H}\n255\n").into_bytes();
                let mut lit = 0usize;
                for (n, q) in px.chunks_exact(4).enumerate() {
                    ppm.extend_from_slice(&q[..3]);
                    lit += usize::from(q[..3] != bg[n * 4..n * 4 + 3]);
                }
                let name = format!("cap_{tier}_{k}.ppm");
                std::fs::write(dir.join(&name), ppm).unwrap();
                lines.push(format!("{name}\t{tier}\t{t}\t{lit}"));
            }
        }
        std::fs::write(dir.join("frames.tsv"), lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn a_meteor_drifts_in_slowly_from_high_up_then_slams_onto_its_shrine() {
        let at = DVec3::new(0.5, 81.0, -169.5);
        let top = meteor_at(at, 40, 0.0);
        assert!(top.y - at.y > 140.0 && (top.x - at.x).hypot(top.z - at.z) > 40.0);
        assert!(meteor_at(at, 40, 1.0).distance(at + DVec3::Y) < 1e-9);
        // Slow at first: half the time covers well under half the way down.
        let half = meteor_at(at, 40, 0.5);
        assert!((top.y - half.y) < 0.3 * (top.y - at.y));
        // The seed turns where it comes from.
        assert!(meteor_at(at, 40, 0.0).distance(meteor_at(at, 220, 0.0)) > 50.0);
    }

    #[test]
    fn shrine_lines_wake_update_crown_and_go() {
        let mut fx = Fx::default();
        let at = DVec3::new(0.5, 81.0, -169.5);
        shrine_heard(&mut fx, "Storm_Shrine", at, "fire", "fall", 0.0, 0.0, 6.0, 40, 5.0);
        assert_eq!(fx.shrines.len(), 1);
        assert!(!fx.shrines[0].landed);
        shrine_heard(&mut fx, "Storm_Shrine", at, "fire", "capture", 0.4, 9.0, 6.0, 40, 5.0);
        assert_eq!((fx.shrines.len(), fx.shrines[0].phase, fx.shrines[0].progress), (1, 2, 0.4));
        // A late joiner long after the landing doesn't hear it again.
        shrine_heard(&mut fx, "Tide_Shrine", at, "water", "awake", 0.0, 60.0, 6.0, 7, 5.0);
        assert!(fx.shrines.iter().find(|s| s.id == "Tide_Shrine").unwrap().landed);
        shrine_heard(&mut fx, "Storm_Shrine", at, "fire", "crown", 1.0, 0.0, 6.0, 40, 0.0);
        assert!(!fx.shrines.iter().any(|s| s.id == "Storm_Shrine"));
        assert!(fx.effects.iter().any(|e| e.shape == Shape::Shrine && e.flags == 4));
        assert_eq!(fx.crowned.len(), 1);
        shrine_heard(&mut fx, "Tide_Shrine", at, "normal", "gone", 0.0, 0.0, 6.0, 0, 0.0);
        assert!(fx.shrines.is_empty());
        // Through the message handler too.
        assert!(heard(&["shrine", "Storm_Shrine", "0.5,81,-169.5", "fire", "fall", "0", "0.5", "6", "40", "5"], None));
        // Only its own shrine goes again: other tests share the global state in parallel.
        assert!(heard(&["shrine", "Storm_Shrine", "0.5,81,-169.5", "normal", "gone", "0", "0", "6", "0", "0"], None));
    }

    #[test]
    fn a_shrine_lands_once_with_a_release_booms_and_a_jolt() {
        let mut fx = Fx::default();
        let at = DVec3::new(0.0, 80.0, 0.0);
        shrine_heard(&mut fx, "S", at, "fire", "fall", 0.0, 6.2, 6.0, 40, 5.0);
        let now = Instant::now();
        shrines_frame(&mut fx, Some(at + DVec3::new(30.0, 2.0, 0.0)), now);
        assert!(fx.effects.iter().any(|e| e.shape == Shape::ZRelease && e.flags == 2));
        assert_eq!(fx.jolts.len(), 1);
        assert_eq!(fx.timed.len(), landing_booms(30.0, "fire").len());
        let n = fx.effects.len();
        shrines_frame(&mut fx, Some(at), now);
        assert_eq!(fx.effects.len(), n, "lands once");
        // Drawn as the column now, with the capture ring's radius.
        let e = &shrine_effects(&fx)[0];
        assert_eq!((e.shape, e.flags, e.radius), (Shape::Shrine, 1, 5.0));
    }

    #[test]
    fn landing_booms_go_by_distance_tier_without_flams() {
        for d in [10.0, 120.0, 400.0] {
            let b = landing_booms(d, "fire");
            assert!(!b.is_empty());
            let mut starts: Vec<f32> = b.iter().filter(|x| x.1 >= 0.8).map(|x| x.3).collect();
            starts.sort_by(f32::total_cmp);
            // Big hits more than 0.15 s apart, except the type's crack layered on the blast.
            assert!(starts.windows(2).all(|w| w[1] - w[0] > 0.15 || w[1] - w[0] < 0.03), "{d}: {starts:?}");
        }
        assert!(landing_booms(400.0, "fire").iter().all(|b| b.0 == "minecraft:entity.lightning_bolt.thunder"));
        assert!(far_falloff(300.0) < 0.4 && far_falloff(10.0) == 1.0);
    }

    #[test]
    fn shake_stacks_as_the_root_of_the_sum_of_squares_and_caps() {
        assert_eq!(shake_total(0.0, 0.0, 0.0, 0.0), 0.0);
        assert!((shake_total(0.6, 0.8, 0.0, 0.0) - 1.0).abs() < 1e-6);
        assert_eq!(shake_total(1.0, 1.0, 1.0, 1.0), 1.0);
        let now = Instant::now();
        let j = [(now, 1.0, 0.5), (now, 1.0, 0.5)];
        assert!((jolts_now(&j, now) - 0.5f32.hypot(0.5)).abs() < 1e-4);
        // A jolt still on its way (the thump travels) adds nothing yet.
        assert_eq!(jolts_now(&[(now + std::time::Duration::from_secs(1), 1.0, 1.0)], now), 0.0);
        for i in 0..200 {
            let v = shake_motion(i as f32 * 0.037, 0.8, 11.0);
            assert!(v.abs() <= 1.5 + 1e-4);
        }
    }
}
