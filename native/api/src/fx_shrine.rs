//! Each type's shrine arrival on the CPU side (docs/fx/SHRINE_FX.md "Arrivals"): where its focus is
//! (the dragon, the eclipse, the ball of energy...) for the effects pass, the coloured light and the
//! sounds; how much the camera rumbles while it builds; and its sounds and jolts, timed from waking.
//! The shader (`sh_arrive` in fx.rs) draws the same paths: `dragon_at`, `strike_times`, `fist_times`
//! and `blade_times` must match it.
//!
//! Sounds are vanilla ids only and follow Orbital Laser's timing rules (docs/fx/ORBITAL_LASER_REFERENCE.md
//! "Sound timing"): each is delayed by its distance (d/343), heard by distance tier (near: all of them;
//! mid: the ones marked `far`, quieter; far: only the big ones), and no two big hits start 0.01-0.15 s
//! apart (they would flam; at the same instant they layer).

use glam::DVec3;

/// Where a cue's sound comes from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum From {
    /// This many blocks over the shrine.
    Up(f64),
    /// The sequence's focus at the cue's time (the dragon's wings).
    Focus,
}

/// One timed sound (and maybe a jolt of the camera) in a type's build-up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cue {
    /// Seconds after waking.
    pub t: f32,
    pub sound: &'static str,
    pub vol: f32,
    pub pitch: f32,
    pub from: From,
    /// Also heard at the middle distance tier.
    pub far: bool,
    /// A big hit: heard even from far off, and never within 0.15 s of another.
    pub big: bool,
    /// Camera jolt strength (0 none), arriving with the ground's thump.
    pub jolt: f32,
}

const fn cue(t: f32, sound: &'static str, vol: f32, pitch: f32, up: f64) -> Cue {
    Cue { t, sound, vol, pitch, from: From::Up(up), far: false, big: false, jolt: 0.0 }
}

impl Cue {
    const fn far(mut self) -> Self {
        self.far = true;
        self
    }
    const fn big(mut self, jolt: f32) -> Self {
        self.big = true;
        self.far = true;
        self.jolt = jolt;
        self
    }
    const fn at_focus(mut self) -> Self {
        self.from = From::Focus;
        self
    }
}

fn smooth(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The electric strikes' times (s after waking): faster and faster, the last 0.3 s before the climax
/// (the shader's `el_time`, i = 1..11).
pub fn strike_times(fall: f32) -> Vec<f32> {
    (1..12).map(|i| fall * (i as f32 / 12.0).powf(0.6)).collect()
}

/// The fighting ki strikes (the shader's `ft_time`).
pub const FIST_TIMES: [f32; 4] = [1.3, 2.7, 3.9, 4.9];
/// The steel blades biting into the ground (the shader's `st_time`).
pub const BLADE_TIMES: [f32; 6] = [0.9, 1.6, 2.25, 2.85, 3.4, 3.9];
/// The poison geyser's eruptions (the shader's `ps_time`).
pub const GEYSER_TIMES: [f32; 3] = [1.6, 3.4, 4.8];

/// The dragon's flight `s` seconds after waking (the shader's `dragon_at`): circling 42 blocks out and
/// 50 up, then up and over into a dive onto the shrine at `fall`.
pub fn dragon_at(base: DVec3, seed: u32, s: f64, fall: f64) -> DVec3 {
    let t1 = fall * 0.7;
    let a0 = (seed % 997) as f64 * 0.0174533;
    let w = 1.3;
    let st = s.min(t1);
    let th = a0 + st * w;
    let p1 = base + DVec3::new(th.cos() * 42.0, 50.0 + 4.0 * (st * 1.7).sin(), th.sin() * 42.0);
    if s <= t1 {
        return p1;
    }
    let u = ((s - t1) / (fall - t1)).clamp(0.0, 1.0);
    let e = u + (u * u - u) * 0.6;
    let c1 = p1 + DVec3::new(-th.sin() * 30.0, 30.0, th.cos() * 30.0);
    let c2 = base + DVec3::new(0.0, 85.0, 0.0);
    let end = base + DVec3::Y;
    let m = 1.0 - e;
    p1 * (m * m * m) + c1 * (3.0 * m * m * e) + c2 * (3.0 * m * e * e) + end * (e * e * e)
}

/// Where a type's arrival is centred `t` seconds after waking (fire: its meteor). The shader draws its
/// eclipse, eye, gate, fist and ball there; the light and the focus's sounds follow it.
pub fn focus(kind: &str, at: DVec3, seed: u32, t: f32, fall: f32) -> DVec3 {
    let k = (t / fall.max(0.1)).clamp(0.0, 1.0);
    let drop = |h: f64, from: f32| at + DVec3::Y * (1.0 + (h - 1.0) * (1.0 - f64::from(smooth(from, 1.0, k)).powi(2)));
    match kind {
        "fire" => super::meteor_at(at, seed, k),
        "dragon" => dragon_at(at, seed, f64::from(t), f64::from(fall)),
        "normal" => drop(24.0, 0.92),
        "dark" => drop(42.0, 0.86),
        "psychic" => drop(44.0, 0.86),
        "ghost" => drop(22.0, 0.86),
        "fighting" => drop(30.0, 0.88),
        "fairy" => drop(16.0, 0.93),
        "rock" => drop(22.0, 0.86),
        "water" => at + DVec3::Y * 34.0,
        "grass" => at + DVec3::Y * 33.0,
        "ground" => at + DVec3::Y * (1.0 + 20.0 * f64::from(smooth(0.42, 0.55, k) * (1.0 - smooth(0.86, 1.0, k)))),
        "flying" => at + DVec3::Y * (1.0 + 61.0 * f64::from(1.0 - smooth(0.08, 0.92, k))),
        "steel" => at + DVec3::Y * (16.0 + 44.0 * f64::from(1.0 - smooth(0.5, 0.95, k))),
        "ice" => at + DVec3::Y * 12.0,
        "bug" => at + DVec3::Y * 6.0,
        _ => at + DVec3::Y * 3.0,
    }
}

/// The camera's rumble near a shrine as its arrival builds (k 0-1 of the build-up), before distance.
pub fn rumble(kind: &str, k: f32) -> f32 {
    match kind {
        // An earthquake from the start.
        "ground" => 0.12 + 0.33 * k,
        "rock" => 0.08 + 0.25 * k * k,
        "electric" | "water" => 0.2 * k * k,
        "fighting" => 0.15 * k,
        "steel" => 0.1 * k,
        // Nothing until it dives.
        "dragon" => 0.32 * smooth(0.72, 1.0, k).powi(2),
        "fire" => 0.3 * k.powi(3),
        _ => 0.22 * k.powi(3),
    }
}

/// A type's build-up sounds and jolts, in time order, flams taken out (`deflam`).
pub fn cues(kind: &str, fall: f32) -> Vec<Cue> {
    let mut v: Vec<Cue> = match kind {
        "normal" => vec![
            cue(0.3, "minecraft:block.beacon.ambient", 2.0, 0.6, 24.0).far(),
            cue(1.5, "minecraft:block.respawn_anchor.charge", 2.0, 0.6, 24.0),
            cue(3.0, "minecraft:block.respawn_anchor.charge", 2.0, 0.75, 24.0),
            cue(4.3, "minecraft:block.respawn_anchor.charge", 2.0, 0.9, 24.0).far(),
            cue(5.2, "minecraft:block.conduit.activate", 2.5, 0.7, 24.0).far(),
            cue(5.75, "minecraft:block.beacon.power_select", 2.5, 0.5, 12.0).big(0.15),
        ],
        "water" => {
            let mut v: Vec<Cue> = [0.4, 1.9, 3.4, 4.9].iter().map(|&t| cue(t, "minecraft:weather.rain", 1.4, 0.8, 10.0).far()).collect();
            v.extend([1.0, 2.5, 4.0].iter().map(|&t| cue(t, "minecraft:block.bubble_column.whirlpool_ambient", 2.5, 0.6, 34.0)));
            v.push(cue(0.2, "minecraft:entity.lightning_bolt.thunder", 1.2, 0.5, 56.0).far());
            v.push(cue(5.2, "minecraft:item.trident.riptide_3", 2.5, 0.6, 20.0).big(0.12));
            v
        }
        "electric" => {
            let mut v = vec![cue(0.2, "minecraft:entity.lightning_bolt.thunder", 1.5, 0.5, 70.0).far()];
            for (i, t) in strike_times(fall).into_iter().enumerate() {
                v.push(cue(t, "minecraft:entity.lightning_bolt.impact", 2.5, 0.8 + 0.03 * i as f32, 2.0).big(0.16));
                if i % 3 == 0 {
                    v.push(cue(t, "minecraft:entity.lightning_bolt.thunder", 2.0, 0.7 + 0.02 * i as f32, 30.0).far());
                }
            }
            v
        }
        "grass" => vec![
            cue(0.4, "minecraft:block.rooted_dirt.break", 2.5, 0.5, 0.0).far(),
            cue(0.7, "minecraft:block.chorus_flower.grow", 2.5, 0.5, 6.0).far(),
            cue(1.4, "minecraft:block.rooted_dirt.break", 2.5, 0.6, 0.0),
            cue(2.0, "minecraft:block.chorus_flower.grow", 2.5, 0.6, 14.0),
            cue(3.0, "minecraft:block.cave_vines.place", 2.5, 0.5, 20.0),
            cue(3.6, "minecraft:block.chorus_flower.grow", 2.5, 0.7, 24.0),
            cue(4.6, "minecraft:block.azalea_leaves.place", 2.5, 0.5, 28.0),
            cue(5.25, "minecraft:item.bone_meal.use", 2.5, 0.6, 28.0).far(),
            cue(5.6, "minecraft:block.cherry_leaves.place", 2.5, 0.7, 28.0),
        ],
        "ice" => vec![
            cue(0.3, "minecraft:item.elytra.flying", 1.5, 0.6, 10.0).far(),
            cue(1.0, "minecraft:entity.player.hurt_freeze", 2.0, 0.5, 2.0),
            cue(1.25, "minecraft:block.amethyst_cluster.place", 2.5, 0.5, 4.0),
            cue(1.95, "minecraft:block.amethyst_cluster.place", 2.5, 0.6, 6.0),
            cue(2.65, "minecraft:block.amethyst_cluster.place", 2.5, 0.7, 8.0),
            cue(3.4, "minecraft:block.amethyst_cluster.place", 2.5, 0.8, 12.0),
            cue(4.1, "minecraft:block.amethyst_block.resonate", 2.5, 0.6, 14.0).far(),
            cue(5.3, "minecraft:entity.player.hurt_freeze", 2.5, 0.4, 12.0),
        ],
        "fighting" => {
            let mut v = vec![cue(0.3, "minecraft:block.respawn_anchor.charge", 2.0, 0.5, 4.0).far()];
            for (i, &t) in FIST_TIMES.iter().enumerate() {
                v.push(cue(t, "minecraft:entity.player.attack.strong", 3.0, 0.5 + 0.05 * i as f32, 1.0).big(0.3));
                v.push(cue(t, "minecraft:entity.iron_golem.attack", 2.5, 0.5, 1.0));
            }
            v.push(cue(5.5, "minecraft:block.respawn_anchor.charge", 2.5, 0.8, 30.0));
            v
        }
        "poison" => {
            let mut v = vec![
                cue(0.3, "minecraft:block.bubble_column.upwards_ambient", 2.5, 0.5, 2.0).far(),
                cue(0.8, "minecraft:block.brewing_stand.brew", 2.5, 0.5, 2.0),
                cue(1.1, "minecraft:block.lava.pop", 2.5, 0.6, 2.0),
                cue(2.4, "minecraft:block.lava.pop", 2.5, 0.5, 2.0),
                cue(4.2, "minecraft:block.lava.pop", 2.5, 0.6, 2.0),
                cue(2.1, "minecraft:block.bubble_column.upwards_ambient", 2.5, 0.6, 2.0),
            ];
            for (i, &t) in GEYSER_TIMES.iter().enumerate() {
                v.push(cue(t, "minecraft:entity.puffer_fish.blow_up", 2.5, 0.5 + 0.05 * i as f32, 4.0).big(0.12));
                v.push(cue(t + 0.3, "minecraft:entity.slime.squish", 2.5, 0.5, 10.0));
            }
            v
        }
        "ground" => vec![
            cue(0.3, "minecraft:entity.warden.dig", 2.5, 0.6, 0.0).far(),
            cue(1.0, "minecraft:block.gravel.break", 2.5, 0.5, 0.0),
            cue(1.8, "minecraft:block.gravel.break", 2.5, 0.6, 0.0),
            cue(2.55, "minecraft:entity.warden.emerge", 2.5, 0.7, 4.0).far().big(0.4),
            cue(3.5, "minecraft:block.rooted_dirt.break", 2.5, 0.5, 20.0),
            cue(4.4, "minecraft:block.gravel.break", 2.5, 0.5, 30.0),
            cue(5.2, "minecraft:block.sand.break", 2.5, 0.5, 20.0).far(),
        ],
        "flying" => vec![
            cue(0.2, "minecraft:item.elytra.flying", 2.0, 0.7, 20.0).far(),
            cue(1.5, "minecraft:entity.breeze.idle_air", 2.5, 0.6, 30.0),
            cue(2.2, "minecraft:entity.phantom.flap", 2.0, 0.6, 20.0),
            cue(3.0, "minecraft:entity.breeze.idle_air", 2.5, 0.5, 20.0),
            cue(3.8, "minecraft:entity.phantom.flap", 2.0, 0.6, 12.0),
            cue(4.6, "minecraft:entity.phantom.flap", 2.0, 0.7, 6.0),
            cue(5.6, "minecraft:entity.breeze.wind_burst", 2.5, 0.6, 3.0).big(0.15),
        ],
        "psychic" => vec![
            cue(0.3, "minecraft:block.portal.trigger", 1.6, 0.7, 10.0).far(),
            cue(1.2, "minecraft:entity.illusioner.prepare_mirror", 2.5, 0.6, 44.0).far(),
            cue(2.4, "minecraft:block.beacon.ambient", 2.0, 0.7, 20.0),
            cue(3.2, "minecraft:entity.evoker.prepare_wololo", 2.5, 0.6, 44.0),
            cue(4.6, "minecraft:block.respawn_anchor.ambient", 2.5, 0.6, 44.0),
            cue(5.3, "minecraft:entity.evoker.cast_spell", 2.5, 0.5, 20.0).big(0.1),
        ],
        "bug" => vec![
            cue(0.2, "minecraft:entity.bee.loop_aggressive", 2.0, 0.6, 10.0).far(),
            cue(1.0, "minecraft:entity.silverfish.ambient", 2.5, 0.6, 4.0),
            cue(1.8, "minecraft:entity.bee.loop_aggressive", 2.0, 0.5, 8.0),
            cue(2.6, "minecraft:entity.silverfish.ambient", 2.5, 0.5, 4.0),
            cue(3.4, "minecraft:entity.bee.loop_aggressive", 2.0, 0.7, 6.0),
            cue(4.0, "minecraft:entity.spider.ambient", 2.5, 0.5, 6.0),
            cue(5.25, "minecraft:entity.turtle.egg_crack", 2.5, 0.5, 6.0),
            cue(5.7, "minecraft:entity.turtle.egg_crack", 2.5, 0.6, 6.0).far(),
        ],
        "rock" => vec![
            cue(0.4, "minecraft:block.stone.break", 2.5, 0.5, 0.0).far(),
            cue(0.9, "minecraft:block.stone.break", 2.5, 0.6, 0.0),
            cue(1.4, "minecraft:block.stone.break", 2.5, 0.5, 0.0),
            cue(1.9, "minecraft:block.stone.break", 2.5, 0.6, 0.0),
            cue(2.4, "minecraft:block.grindstone.use", 2.5, 0.5, 12.0).far(),
            cue(4.0, "minecraft:block.basalt.break", 2.5, 0.5, 22.0).big(0.15),
            cue(4.4, "minecraft:block.basalt.break", 2.5, 0.45, 22.0).big(0.15),
            cue(4.75, "minecraft:block.basalt.break", 2.5, 0.4, 22.0).big(0.2),
        ],
        "ghost" => {
            let mut v = vec![
                cue(0.2, "minecraft:ambient.soul_sand_valley.mood", 2.5, 0.6, 2.0).far(),
                cue(2.0, "minecraft:entity.ghast.ambient", 1.6, 0.5, 22.0).far(),
                cue(4.6, "minecraft:entity.vex.charge", 2.5, 0.5, 22.0),
                cue(5.2, "minecraft:block.trial_spawner.ominous_activate", 2.5, 0.7, 22.0).big(0.1),
            ];
            v.extend([0.8, 1.5, 2.6, 3.3, 4.0].iter().enumerate().map(|(i, &t)| cue(t, "minecraft:particle.soul_escape", 2.5, 0.6 + 0.07 * i as f32, 1.0)));
            v
        }
        "dragon" => {
            let mut v = vec![cue(0.4, "minecraft:entity.ender_dragon.growl", 3.0, 0.8, 0.0).at_focus().far()];
            let mut t = 0.7;
            while t < fall * 0.72 {
                v.push(cue(t, "minecraft:entity.ender_dragon.flap", 2.5, 0.7, 0.0).at_focus());
                t += 0.75;
            }
            v.push(cue(fall * 0.72, "minecraft:entity.ender_dragon.growl", 3.0, 0.65, 0.0).at_focus().big(0.1));
            v
        }
        "dark" => {
            let mut v: Vec<Cue> = [0.8, 1.9, 2.8, 3.6, 4.2, 4.7, 5.1, 5.45, 5.75].iter().map(|&t| cue(t, "minecraft:entity.warden.heartbeat", 2.5, 0.6, 2.0)).collect();
            v.extend([0.4, 1.4, 2.3].iter().map(|&t| cue(t, "minecraft:block.sculk.spread", 2.5, 0.6, 0.0)));
            v.push(cue(3.2, "minecraft:block.sculk_shrieker.shriek", 2.5, 0.6, 42.0).far());
            v
        }
        "steel" => {
            let mut v: Vec<Cue> = BLADE_TIMES.iter().enumerate().map(|(i, &t)| cue(t, "minecraft:block.anvil.land", 2.5, 0.5 + 0.04 * i as f32, 0.0).big(0.2)).collect();
            v.push(cue(3.15, "minecraft:block.chain.place", 2.5, 0.5, 40.0));
            v.push(cue(4.5, "minecraft:block.smithing_table.use", 2.5, 0.6, 20.0));
            v.push(cue(5.6, "minecraft:block.heavy_core.place", 2.5, 0.5, 16.0).big(0.15));
            v
        }
        "fairy" => {
            let mut v: Vec<Cue> = (0..12).map(|i| cue(0.5 + 0.4 * i as f32, "minecraft:block.amethyst_block.chime", 2.0, 0.6 + 0.08 * i as f32, 16.0)).collect();
            v.push(cue(2.0, "minecraft:entity.allay.ambient_without_item", 2.5, 0.8, 16.0).far());
            v.push(cue(3.6, "minecraft:entity.allay.ambient_without_item", 2.5, 0.9, 16.0));
            v.push(cue(4.5, "minecraft:block.amethyst_block.resonate", 2.5, 0.8, 16.0).far());
            v.push(cue(5.6, "minecraft:entity.allay.item_given", 2.5, 0.8, 16.0));
            v
        }
        // Fire's meteor has the beacon heads-up and the landing alone.
        _ => Vec::new(),
    };
    v.retain(|c| c.t >= 0.0 && c.t < fall);
    deflam(v)
}

/// Time order, and no big hit starting 0.01-0.15 s after another kept one (the later is dropped).
pub fn deflam(mut v: Vec<Cue>) -> Vec<Cue> {
    v.sort_by(|a, b| a.t.total_cmp(&b.t));
    let mut last: Option<f32> = None;
    v.retain(|c| {
        if !c.big {
            return true;
        }
        if let Some(l) = last {
            let d = c.t - l;
            if d > 0.01 && d < 0.15 {
                return false;
            }
        }
        last = Some(c.t);
        true
    });
    v
}

/// The type's own hit at its climax, 0.38 s in (between the landing's thunder and the cry).
pub fn climax_sound(kind: &str) -> Option<(&'static str, f32, f32)> {
    Some(match kind {
        "normal" => ("minecraft:block.beacon.power_select", 3.0, 0.6),
        "water" => ("minecraft:entity.generic.splash", 3.0, 0.5),
        "electric" => ("minecraft:item.trident.thunder", 3.0, 0.8),
        "grass" => ("minecraft:item.bone_meal.use", 3.0, 0.5),
        "ice" => ("minecraft:block.glass.break", 3.0, 0.5),
        "fighting" => ("minecraft:entity.warden.attack_impact", 3.0, 0.6),
        "poison" => ("minecraft:entity.puffer_fish.blow_up", 3.0, 0.4),
        "ground" => ("minecraft:block.mud.break", 3.0, 0.4),
        "flying" => ("minecraft:entity.breeze.wind_burst", 3.0, 0.5),
        "psychic" => ("minecraft:entity.evoker.cast_spell", 3.0, 0.4),
        "bug" => ("minecraft:block.sniffer_egg.hatch", 3.0, 0.6),
        "rock" => ("minecraft:block.basalt.break", 3.0, 0.35),
        "ghost" => ("minecraft:block.trial_spawner.ominous_activate", 3.0, 0.5),
        "dragon" => ("minecraft:entity.ender_dragon.growl", 3.0, 1.0),
        "dark" => ("minecraft:entity.warden.sonic_boom", 2.5, 0.7),
        "steel" => ("minecraft:block.anvil.land", 3.0, 0.4),
        "fairy" => ("minecraft:block.amethyst_cluster.break", 3.0, 0.6),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TYPES: [&str; 18] = [
        "normal", "fire", "water", "electric", "grass", "ice", "fighting", "poison", "ground", "flying", "psychic", "bug",
        "rock", "ghost", "dragon", "dark", "steel", "fairy",
    ];

    #[test]
    fn every_type_but_fire_has_its_own_build_up_sounds_and_none_flam() {
        for kind in TYPES {
            let v = cues(kind, 6.0);
            if kind == "fire" {
                assert!(v.is_empty());
                continue;
            }
            assert!(v.len() >= 5, "{kind}: {} cues", v.len());
            assert!(v.windows(2).all(|w| w[0].t <= w[1].t), "{kind}: out of order");
            assert!(v.iter().all(|c| c.t >= 0.0 && c.t < 6.0 && c.sound.starts_with("minecraft:")), "{kind}");
            assert!(v.iter().any(|c| c.far), "{kind}: nothing heard from further off");
            let big: Vec<f32> = v.iter().filter(|c| c.big).map(|c| c.t).collect();
            for w in big.windows(2) {
                let d = w[1] - w[0];
                assert!(d <= 0.01 || d >= 0.15, "{kind}: big hits {d} s apart");
            }
            assert!(climax_sound(kind).is_some(), "{kind}");
        }
        assert!(climax_sound("fire").is_none());
    }

    #[test]
    fn deflam_drops_a_hit_too_close_after_another_but_keeps_layers() {
        let a = cue(1.0, "minecraft:a", 1.0, 1.0, 0.0).big(0.0);
        let b = cue(1.08, "minecraft:b", 1.0, 1.0, 0.0).big(0.0);
        let c = cue(1.0, "minecraft:c", 1.0, 1.0, 0.0).big(0.0);
        let d = cue(1.3, "minecraft:d", 1.0, 1.0, 0.0).big(0.0);
        let v = deflam(vec![d, b, a, c]);
        let names: Vec<&str> = v.iter().map(|c| c.sound).collect();
        assert_eq!(names, ["minecraft:a", "minecraft:c", "minecraft:d"]);
    }

    #[test]
    fn electric_strikes_speed_up_but_never_flam_and_end_before_the_climax() {
        let s = strike_times(6.0);
        assert_eq!(s.len(), 11);
        let gaps: Vec<f32> = s.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(gaps.windows(2).all(|g| g[1] < g[0]), "{gaps:?}");
        assert!(gaps.iter().all(|g| *g > 0.25), "{gaps:?}");
        assert!(*s.last().unwrap() < 5.8);
        // At most a few strikes a second, even at the end (photosensitivity: bolts, never the whole picture).
        assert!(1.0 / gaps.last().unwrap() < 3.5);
    }

    #[test]
    fn the_dragon_circles_high_then_dives_onto_the_shrine() {
        let at = DVec3::new(0.5, 81.0, -169.5);
        for s in [0.0, 1.0, 2.0, 3.0, 4.0] {
            let p = dragon_at(at, 40, s, 6.0);
            let r = ((p.x - at.x).powi(2) + (p.z - at.z).powi(2)).sqrt();
            assert!((r - 42.0).abs() < 0.01 && p.y - at.y > 40.0, "{s}: {p}");
        }
        assert!(dragon_at(at, 40, 6.0, 6.0).distance(at + DVec3::Y) < 1e-6);
        // It climbs before it falls.
        assert!(dragon_at(at, 40, 4.8, 6.0).y > dragon_at(at, 40, 4.2, 6.0).y);
        // No jump where the dive starts.
        assert!(dragon_at(at, 40, 4.2, 6.0).distance(dragon_at(at, 40, 4.2001, 6.0)) < 0.05);
    }

    #[test]
    fn every_focus_ends_on_or_over_the_shrine_and_rumble_stays_gentle() {
        let at = DVec3::new(0.0, 81.0, -170.0);
        for kind in TYPES {
            let end = focus(kind, at, 40, 6.0, 6.0);
            assert!((end.x - at.x).abs() < 1e-6 && (end.z - at.z).abs() < 1e-6, "{kind}: {end}");
            assert!(end.y >= at.y && end.y < at.y + 40.0, "{kind}: {end}");
            for k in [0.0, 0.5, 1.0] {
                let r = rumble(kind, k);
                assert!((0.0..=0.5).contains(&r), "{kind} {k}: {r}");
            }
        }
        // The ones that come down onto it do.
        for kind in ["fire", "dragon", "normal", "dark", "psychic", "ghost", "fighting", "fairy", "rock"] {
            assert!(focus(kind, at, 40, 6.0, 6.0).distance(at + DVec3::Y) < 1e-6, "{kind}");
        }
    }
}
