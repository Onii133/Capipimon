//! Made-up attack poses for species whose model has no `physical` / `special` animation (most of
//! Cobblemon's: it builds many of those in code, not in the files we import).
//!
//! `Rig::of` reads the model's bones once (by name, the way Cobblemon's modellers name them) and
//! sorts the body into a plan: biped, quadruped, serpent, flyer or blob. `Rig::pose` then gives
//! offsets for the bones it found (file degrees, added on top of the idle) and a whole-body move
//! (lean, lunge, rise, squash) at a strength from `amount`. Every species gets one: a model with
//! no named parts at all still lunges and squashes as a whole.
//!
//! File-degree conventions (Blockbench, as the Palkia trial was tuned): +x tips a bone forward /
//! down (a head nods, a jaw opens), -x raises it (an arm reaches forward, a head looks up);
//! z spreads limbs sideways (times the side: +1 left, -1 right); y turns (a tail lash).

use crate::bedrock::Model;
use glam::Vec3;

/// The body plan a model's bones suggest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Arms, standing on two legs or none (Machamp, Gardevoir, Palkia).
    Biped,
    /// Four or more legs, no arms (Arcanine, Rhydon's kin, Ponyta).
    Quadruped,
    /// No legs or arms, a long body or tail (Ekans, Gyarados, Magikarp).
    Serpent,
    /// Wings and no arms (Pidgeot, Zubat, Butterfree).
    Flyer,
    /// A short body with a tail or fins and no limbs (Magikarp, Goldeen, Basculin).
    Fish,
    /// Nothing to move but the body (Voltorb, Ditto, Grimer).
    Blob,
}

/// The bones a stand-in pose moves, found once per model.
#[derive(Clone, Debug)]
pub struct Rig {
    pub plan: Plan,
    torso: Option<usize>,
    /// Neck bones from the body out (neck, neck2, ...).
    necks: Vec<usize>,
    head: Option<usize>,
    jaw: Option<usize>,
    /// Top arm bone, side (+1 left, -1 right), and the bone below it (the elbow).
    arms: Vec<(usize, f32, Option<usize>)>,
    /// Top leg bones and their side; front legs first for quadrupeds.
    front_legs: Vec<(usize, f32)>,
    back_legs: Vec<(usize, f32)>,
    wings: Vec<(usize, f32)>,
    /// Side fins / flippers and their side.
    fins: Vec<(usize, f32)>,
    /// The tail chain from the body out (tail, tail2, ...), at most 6.
    tail: Vec<usize>,
}

/// A stand-in pose: bone offsets plus how the whole body moves about its feet.
#[derive(Clone, Debug, Default)]
pub struct Pose {
    pub offsets: Vec<(usize, Vec3)>,
    /// Lean, degrees (forward +).
    pub lean: f32,
    /// Push forward / up, fractions of the body height.
    pub fwd: f32,
    pub up: f32,
    /// Squash and stretch: the body is drawn `1 + squash` tall, `1 - squash / 2` wide and
    /// `1 - squash` long (negative: flatter and longer, as in a lunge).
    pub squash: f32,
}

fn side(k: &str) -> f32 {
    if k.contains("left") || k.ends_with("_l") || k.starts_with("l_") {
        1.0
    } else if k.contains("right") || k.ends_with("_r") || k.starts_with("r_") {
        -1.0
    } else {
        0.0
    }
}

/// The side of limb bone `k` of kind `part`, also for short names (`lleg`, `armr`, `rwing2`).
fn limb_side(k: &str, part: &str) -> f32 {
    let s = side(k);
    if s != 0.0 {
        return s;
    }
    let bare = k.trim_end_matches(|c: char| c.is_ascii_digit() || c == '_');
    if bare.starts_with(&format!("l{part}")) || bare.ends_with(&format!("{part}l")) {
        1.0
    } else if bare.starts_with(&format!("r{part}")) || bare.ends_with(&format!("{part}r")) {
        -1.0
    } else {
        0.0
    }
}

/// A limb bone named `part` (arm, leg, wing): not the group that holds them all (`arms`), not
/// armour, not a locator.
fn limb(k: &str, part: &str) -> bool {
    let group = format!("{part}s");
    k.contains(part) && !k.contains(&group) && !k.contains("armor") && !k.contains("armour") && !k.starts_with("locator")
}

impl Rig {
    pub fn of(model: &Model) -> Self {
        let bones = &model.bones;
        let key = |i: usize| bones[i].key.as_str();
        let find = |names: &[&str]| names.iter().find_map(|n| bones.iter().position(|b| b.key == *n));
        let ancestor = |a: usize, mut i: usize| {
            while let Some(p) = bones[i].parent {
                if p == a {
                    return true;
                }
                i = p;
            }
            false
        };
        // A bone whose pivot sits far from everything it carries would swing that off into the
        // air when turned (Qwilfish's mouth pivots 20 px below its lips): leave those still.
        let sane = |i: usize| {
            let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
            for (j, b) in bones.iter().enumerate() {
                if j == i || ancestor(i, j) {
                    for c in &b.cubes {
                        lo = lo.min(c.min);
                        hi = hi.max(c.max);
                    }
                }
            }
            if lo.x > hi.x {
                return true;
            }
            let pivot = bones[i].pivot;
            let gap = (lo - pivot).max(pivot - hi).max(Vec3::ZERO).length();
            gap <= (0.6 * (hi - lo).length()).max(4.0)
        };
        // The top bone of each limb: no bone above it is the same kind of limb.
        let tops = |part: &str| -> Vec<(usize, f32)> {
            (0..bones.len())
                .filter(|&i| limb(key(i), part) && limb_side(key(i), part) != 0.0)
                .filter(|&i| {
                    let mut p = bones[i].parent;
                    while let Some(j) = p {
                        if limb(key(j), part) && limb_side(key(j), part) != 0.0 {
                            return false;
                        }
                        p = bones[j].parent;
                    }
                    true
                })
                .filter(|&i| sane(i))
                .map(|i| (i, limb_side(key(i), part)))
                .collect()
        };
        let head = find(&["head"])
            .or_else(|| bones.iter().position(|b| b.key.starts_with("head") && !b.key.contains("ai") && !b.key.contains("angle") && !b.cubes.is_empty()))
            .filter(|&i| sane(i));
        let jaw = find(&["jaw", "lower_jaw", "jaw_lower", "jawbot", "jaw_bottom", "lowerjaw", "mouth", "mouth_lower", "lower_lip"]).filter(|&i| sane(i));
        // Arms: the elbow is the first arm-ish bone under the top one (arm_right -> arm_right2,
        // or forearm / lower_arm / hand).
        let arms: Vec<(usize, f32, Option<usize>)> = tops("arm")
            .into_iter()
            .map(|(i, s)| {
                let under = |j: usize| j != i && ancestor(i, j) && !key(j).starts_with("locator") && !key(j).contains("armor");
                let lower = (0..bones.len())
                    .find(|&j| under(j) && (key(j).contains("forearm") || key(j).contains("lower_arm")))
                    .or_else(|| (0..bones.len()).find(|&j| under(j) && limb(key(j), "arm")))
                    .or_else(|| (0..bones.len()).find(|&j| under(j) && key(j).contains("hand")));
                (i, s, lower)
            })
            .collect();
        let legs = tops("leg");
        let back = |k: &str| k.contains("back") || k.contains("hind") || k.contains("rear");
        let named_front_back = legs.iter().any(|&(i, _)| back(key(i))) && legs.iter().any(|&(i, _)| !back(key(i)));
        let (mut front_legs, mut back_legs): (Vec<_>, Vec<_>) = if named_front_back {
            legs.iter().partition(|&&(i, _)| !back(key(i)))
        } else if legs.len() >= 4 && arms.is_empty() {
            // Four legs with plain names: split by how far forward the hip is (the model faces -z
            // in viewport space, so smaller z is further forward).
            let mid = legs.iter().map(|&(i, _)| bones[i].pivot.z).sum::<f32>() / legs.len() as f32;
            legs.iter().partition(|&&(i, _)| bones[i].pivot.z <= mid)
        } else {
            (legs.clone(), Vec::new())
        };
        let mut wings = tops("wing");
        wings.truncate(4);
        // Side fins and flippers (not fingers).
        let mut fins: Vec<(usize, f32)> = (0..bones.len())
            .filter(|&i| {
                let k = key(i);
                (k.contains("fin") && !k.contains("finger") || k.contains("flipper")) && side(k) != 0.0 && !k.starts_with("locator")
            })
            .filter(|&i| bones[i].parent.is_none_or(|p| !(key(p).contains("fin") && side(key(p)) != 0.0)))
            .filter(|&i| sane(i))
            .map(|i| (i, side(key(i))))
            .collect();
        fins.truncate(6);
        // Chains: neck, neck2, ... / tail, tailseg1, ... / segment1, segment2, ... in the order they
        // hang from each other: from the first bone named like one of `roots`, follow the child
        // that is named like it too (not a side part, a tip locator or a spike).
        let chain = |roots: &[&str], like: &[&str]| -> Vec<usize> {
            let mut out: Vec<usize> = Vec::new();
            let start = roots.iter().find_map(|r| {
                bones.iter().position(|b| b.key == *r).or_else(|| bones.iter().position(|b| b.key == format!("{r}1")))
            });
            let Some(mut at) = start else { return out };
            out.push(at);
            while out.len() < 8 {
                let next = (0..bones.len()).find(|&j| {
                    let k = key(j);
                    bones[j].parent == Some(at)
                        && like.iter().any(|l| k.starts_with(l))
                        && side(k) == 0.0
                        && !k.contains("tip")
                        && !k.contains("spike")
                        && !k.contains("fin")
                });
                match next {
                    Some(n) => {
                        out.push(n);
                        at = n;
                    }
                    None => break,
                }
            }
            // Skip links that would swing off (a group bone pivoting at the feet); the ones
            // past it still bend.
            out.retain(|&i| sane(i));
            out
        };
        let necks = chain(&["neck", "lowernecc"], &["neck", "midnecc", "lowernecc"]);
        let mut tail = chain(&["tail", "tail_middle", "tail_main", "tailseg"], &["tail", "seg"]);
        if tail.len() < 3 {
            // A body made of segments (Onix, centipedes) whips like a tail.
            let segs = chain(&["segment", "body_segment", "seg"], &["segment", "body_segment", "seg"]);
            if segs.len() >= 3 {
                tail = segs;
            }
        }
        // The torso to bend: not one that carries the legs (bending that would bend the stance;
        // the whole-body lean does that job instead).
        let carries_legs = |t: usize| front_legs.iter().chain(&back_legs).any(|&(l, _)| ancestor(t, l));
        let torso = ["torso", "upper_body", "chest", "body", "torso2"]
            .iter()
            .filter_map(|n| find(&[n]))
            .find(|&t| !carries_legs(t) && head.is_none_or(|h| h != t) && sane(t));
        let plan = if !arms.is_empty() {
            Plan::Biped
        } else if front_legs.len() + back_legs.len() >= 4 || (!back_legs.is_empty() && !front_legs.is_empty()) {
            Plan::Quadruped
        } else if !wings.is_empty() {
            Plan::Flyer
        } else if front_legs.len() + back_legs.len() >= 2 {
            Plan::Biped
        } else if necks.len() >= 2 || tail.len() >= 4 {
            Plan::Serpent
        } else if !tail.is_empty() || !fins.is_empty() {
            Plan::Fish
        } else {
            Plan::Blob
        };
        if plan != Plan::Quadruped {
            // Bipeds' legs all step the same way; keep them in one list.
            front_legs.append(&mut back_legs);
        }
        Rig { plan, torso, necks, head, jaw, arms, front_legs, back_legs, wings, fins, tail }
    }

    /// The stand-in pose at strength `a` (from `amount`: -0.3 coiled .. 1 struck / reared).
    pub fn pose(&self, special: bool, a: f32) -> Pose {
        let mut p = Pose::default();
        let mut add = |i: Option<usize>, d: Vec3| {
            if let Some(i) = i {
                p.offsets.push((i, d));
            }
        };
        let v = Vec3::new;
        let open = a.max(0.0);
        // A neck chain shares its bend; a head on top takes its own.
        let necks = |add: &mut dyn FnMut(Option<usize>, Vec3), deg: f32| {
            let n = self.necks.len().max(1) as f32;
            for &i in &self.necks {
                add(Some(i), v(deg / n, 0.0, 0.0));
            }
        };
        // A tail chain: `lift` (x) and `lash` (y) spread down it, growing toward the tip.
        let tail = |add: &mut dyn FnMut(Option<usize>, Vec3), lift: f32, lash: f32| {
            // Weights 1, 1.35, 1.7, ... scaled to add up to 1.3 (a long tail curls a bit more than
            // a short one bends).
            let n = self.tail.len();
            let total: f32 = (0..n).map(|k| 1.0 + 0.35 * k as f32).sum::<f32>().max(1.0);
            let reach = if n >= 3 { 1.3 } else { 1.0 };
            for (k, &i) in self.tail.iter().enumerate() {
                let w = (1.0 + 0.35 * k as f32) / total * reach;
                add(Some(i), v(lift * w, lash * w, 0.0));
            }
        };
        match (self.plan, special) {
            // Physical, two legs: coil, then lunge with the arms driving forward (the approved
            // Palkia pose), a step with the legs and a tail lash.
            (Plan::Biped, false) => {
                add(self.torso, v(16.0 * a, 0.0, 0.0));
                necks(&mut add, 10.0 * a);
                add(self.head, v(10.0 * a, 0.0, 0.0));
                add(self.jaw, v(14.0 * open, 0.0, 0.0));
                for &(i, s, lower) in &self.arms {
                    add(Some(i), v(-60.0 * a, 0.0, -8.0 * s * a));
                    add(lower, v(-25.0 * a, 0.0, 0.0));
                }
                for (k, &(i, _)) in self.front_legs.iter().enumerate() {
                    add(Some(i), v(if k % 2 == 0 { -22.0 } else { 14.0 } * a, 0.0, 0.0));
                }
                for &(i, s) in &self.wings {
                    add(Some(i), v(0.0, 0.0, 15.0 * s * a));
                }
                tail(&mut add, 0.0, 30.0 * a);
                p.lean = if self.arms.is_empty() { 14.0 } else { 8.0 } * a;
                p.fwd = 0.22 * a;
                p.squash = 0.04 * a;
            }
            // Special, two legs: rear back, head up, jaw open, arms spread (Palkia).
            (Plan::Biped, true) => {
                add(self.torso, v(-14.0 * a, 0.0, 0.0));
                necks(&mut add, -14.0 * a);
                add(self.head, v(-18.0 * a, 0.0, 0.0));
                add(self.jaw, v(22.0 * a, 0.0, 0.0));
                for &(i, s, lower) in &self.arms {
                    add(Some(i), v(-30.0 * a, 0.0, -55.0 * s * a));
                    add(lower, v(-15.0 * a, 0.0, -10.0 * s * a));
                }
                for &(i, s) in &self.wings {
                    add(Some(i), v(0.0, 0.0, -30.0 * s * a));
                }
                tail(&mut add, -20.0 * a, 0.0);
                p.lean = if self.arms.is_empty() { -12.0 } else { -6.0 } * a;
                p.up = 0.04 * open;
            }
            // Physical, four legs: crouch, then pounce, head down to ram or bite, forelegs
            // reaching, hind legs pushing off, a little hop.
            (Plan::Quadruped, false) => {
                add(self.torso, v(6.0 * a, 0.0, 0.0));
                necks(&mut add, 16.0 * a);
                add(self.head, v(18.0 * a, 0.0, 0.0));
                add(self.jaw, v(26.0 * open, 0.0, 0.0));
                for &(i, _) in &self.front_legs {
                    add(Some(i), v(-38.0 * a, 0.0, 0.0));
                }
                for &(i, _) in &self.back_legs {
                    add(Some(i), v(28.0 * a, 0.0, 0.0));
                }
                for &(i, s) in &self.wings {
                    add(Some(i), v(0.0, 0.0, 20.0 * s * a));
                }
                tail(&mut add, -12.0 * a, 0.0);
                p.lean = 7.0 * a;
                p.fwd = 0.28 * a;
                p.up = 0.1 * open;
                p.squash = 0.06 * a;
            }
            // Special, four legs: rear up on the hind legs, head thrown back, mouth wide.
            (Plan::Quadruped, true) => {
                necks(&mut add, -22.0 * a);
                add(self.head, v(-12.0 * a, 0.0, 0.0));
                add(self.jaw, v(30.0 * a, 0.0, 0.0));
                for &(i, _) in &self.front_legs {
                    add(Some(i), v(-34.0 * a, 0.0, 0.0));
                }
                for &(i, _) in &self.back_legs {
                    add(Some(i), v(10.0 * a, 0.0, 0.0));
                }
                for &(i, s) in &self.wings {
                    add(Some(i), v(0.0, 0.0, -40.0 * s * a));
                }
                tail(&mut add, -25.0 * a, 0.0);
                p.lean = -16.0 * a;
                p.up = 0.03 * open;
            }
            // Physical, no limbs but a long body: draw back, then strike with the head while the
            // tail whips the other way.
            (Plan::Serpent, false) => {
                add(self.torso, v(10.0 * a, 0.0, 0.0));
                necks(&mut add, 24.0 * a);
                add(self.head, v(14.0 * a, 0.0, 0.0));
                add(self.jaw, v(30.0 * open, 0.0, 0.0));
                tail(&mut add, 0.0, 45.0 * a);
                p.lean = 10.0 * a;
                p.fwd = 0.3 * a;
                p.squash = -0.06 * a;
            }
            // Special, long body: rise up tall, head level, mouth open, tail coiled.
            (Plan::Serpent, true) => {
                add(self.torso, v(-12.0 * a, 0.0, 0.0));
                necks(&mut add, -26.0 * a);
                add(self.head, v(16.0 * a, 0.0, 0.0));
                add(self.jaw, v(30.0 * a, 0.0, 0.0));
                for &(i, s) in &self.fins {
                    add(Some(i), v(0.0, 0.0, -30.0 * s * a));
                }
                tail(&mut add, 0.0, -35.0 * a);
                p.lean = -12.0 * a;
                p.up = 0.08 * open;
                p.squash = 0.06 * a;
            }
            // Physical, wings: wings fold back, dive at the target.
            (Plan::Flyer, false) => {
                add(self.torso, v(10.0 * a, 0.0, 0.0));
                necks(&mut add, 8.0 * a);
                add(self.head, v(8.0 * a, 0.0, 0.0));
                add(self.jaw, v(18.0 * open, 0.0, 0.0));
                for &(i, s) in &self.wings {
                    add(Some(i), v(0.0, -35.0 * s * a, 30.0 * s * a));
                }
                for &(i, _) in self.front_legs.iter().chain(&self.back_legs) {
                    add(Some(i), v(-30.0 * a, 0.0, 0.0));
                }
                tail(&mut add, -10.0 * a, 0.0);
                p.lean = 18.0 * a;
                p.fwd = 0.3 * a;
                p.up = -0.04 * open;
            }
            // Special, wings: rise, wings spread wide and high, head back.
            (Plan::Flyer, true) => {
                add(self.torso, v(-10.0 * a, 0.0, 0.0));
                necks(&mut add, -12.0 * a);
                add(self.head, v(-12.0 * a, 0.0, 0.0));
                add(self.jaw, v(24.0 * a, 0.0, 0.0));
                for &(i, s) in &self.wings {
                    add(Some(i), v(0.0, 0.0, -50.0 * s * a));
                }
                tail(&mut add, -20.0 * a, 0.0);
                p.lean = -10.0 * a;
                p.up = 0.12 * open;
            }
            // Physical, fish: tail flick and a dart forward, fins swept back.
            (Plan::Fish, false) => {
                add(self.torso, v(4.0 * a, 0.0, 0.0));
                add(self.head, v(6.0 * a, 0.0, 0.0));
                add(self.jaw, v(28.0 * open, 0.0, 0.0));
                for &(i, s) in &self.fins {
                    add(Some(i), v(0.0, 25.0 * s * a, 0.0));
                }
                tail(&mut add, 0.0, 40.0 * a);
                p.lean = 3.0 * a;
                p.fwd = 0.35 * a;
                p.up = 0.06 * open;
                p.squash = -0.05 * a;
            }
            // Special, fish: nose up, mouth wide, fins flared, tail cocked.
            (Plan::Fish, true) => {
                add(self.torso, v(-4.0 * a, 0.0, 0.0));
                add(self.head, v(-6.0 * a, 0.0, 0.0));
                add(self.jaw, v(30.0 * a, 0.0, 0.0));
                for &(i, s) in &self.fins {
                    add(Some(i), v(0.0, 0.0, -35.0 * s * a));
                }
                tail(&mut add, 0.0, -20.0 * a);
                p.lean = -8.0 * a;
                p.up = 0.08 * open;
            }
            // Physical, nothing to move: squash down, then spring forward stretched.
            (Plan::Blob, false) => {
                add(self.torso, v(10.0 * a, 0.0, 0.0));
                add(self.head, v(10.0 * a, 0.0, 0.0));
                add(self.jaw, v(20.0 * open, 0.0, 0.0));
                for &(i, s) in &self.wings {
                    add(Some(i), v(0.0, 0.0, 25.0 * s * a));
                }
                tail(&mut add, 0.0, 25.0 * a);
                p.lean = 12.0 * a;
                p.fwd = 0.32 * a;
                p.up = 0.08 * open;
                // Crouch flat while coiling, then spring long and low.
                p.squash = if a < 0.0 { 0.5 * a } else { -0.15 * a };
            }
            // Special, nothing to move: puff up and rise, gathering power.
            (Plan::Blob, true) => {
                add(self.torso, v(-8.0 * a, 0.0, 0.0));
                add(self.head, v(-12.0 * a, 0.0, 0.0));
                add(self.jaw, v(24.0 * a, 0.0, 0.0));
                for &(i, s) in &self.wings {
                    add(Some(i), v(0.0, 0.0, -35.0 * s * a));
                }
                tail(&mut add, -20.0 * a, 0.0);
                p.lean = -6.0 * a;
                p.up = 0.1 * open;
                p.squash = 0.18 * a;
            }
        }
        p
    }

    /// One line for checking: the plan and which parts were found.
    pub fn describe(&self, model: &Model) -> String {
        let n = |i: Option<usize>| i.map_or("-".to_string(), |i| model.bones[i].key.clone());
        format!(
            "{:?} torso={} head={} jaw={} necks={} arms={} front={} back={} wings={} fins={} tail={}",
            self.plan,
            n(self.torso),
            n(self.head),
            n(self.jaw),
            self.necks.len(),
            self.arms.len(),
            self.front_legs.len(),
            self.back_legs.len(),
            self.wings.len(),
            self.fins.len(),
            self.tail.len()
        )
    }
}

/// How long a stand-in pose lasts for a wind-up of `w` seconds.
pub fn secs(special: bool, w: f32) -> f32 {
    if special { w.max(0.25) + 0.5 } else { w.max(0.08) + 0.48 }
}

/// Strength (-0.3..1) of the stand-in pose `e` seconds into a cast with a `w` s wind-up.
/// Physical: coils back a little through the wind-up, lunges at the release (0.1 s), holds a
/// moment, eases back over 0.3 s. Special: rears up over 0.25 s, holds through the wind-up and a
/// beat after the release, eases back over 0.35 s.
pub fn amount(special: bool, e: f32, w: f32) -> f32 {
    let ease = |x: f32| {
        let x = x.clamp(0.0, 1.0);
        x * x * (3.0 - 2.0 * x)
    };
    if special {
        let hold = w.max(0.25) + 0.15;
        if e < 0.25 { ease(e / 0.25) } else if e < hold { 1.0 } else { 1.0 - ease((e - hold) / 0.35) }
    } else {
        let w = w.max(0.08);
        if e < w {
            -0.3 * ease(e / w)
        } else if e < w + 0.1 {
            -0.3 + 1.3 * ease((e - w) / 0.1)
        } else if e < w + 0.18 {
            1.0
        } else {
            1.0 - ease((e - w - 0.18) / 0.3)
        }
    }
}

/// A charged special trembles while it holds: a small sideways shake (degrees) at `t` seconds.
pub fn tremble(special: bool, a: f32, t: f32) -> f32 {
    if special && a > 0.95 { (t * 55.0).sin() * 1.2 } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny model from `(name, parent)` pairs, every bone with one cube.
    fn model(bones: &[(&str, &str, [f32; 3])]) -> Model {
        let list: Vec<serde_json::Value> = bones
            .iter()
            .map(|(n, p, pivot)| {
                let mut b = serde_json::json!({"name": n, "pivot": pivot, "cubes": [{"origin": pivot, "size": [1, 1, 1], "uv": [0, 0]}]});
                if !p.is_empty() {
                    b["parent"] = serde_json::json!(p);
                }
                b
            })
            .collect();
        let geo = serde_json::json!({"minecraft:geometry": [{"description": {"texture_width": 16, "texture_height": 16}, "bones": list}]});
        Model::parse(&geo.to_string()).unwrap()
    }

    fn moved(m: &Model, p: &Pose, name: &str) -> bool {
        p.offsets.iter().any(|(i, d)| m.bones[*i].key == name && d.length() > 0.1)
    }

    #[test]
    fn cobblemon_biped_arm_and_elbow() {
        let m = model(&[
            ("body", "", [0.0; 3]),
            ("torso", "body", [0.0, 10.0, 0.0]),
            ("head", "torso", [0.0, 20.0, 0.0]),
            ("jaw", "head", [0.0, 20.0, 0.0]),
            ("arm_left", "torso", [3.0, 18.0, 0.0]),
            ("arm_left2", "arm_left", [4.0, 14.0, 0.0]),
            ("arm_right", "torso", [-3.0, 18.0, 0.0]),
            ("arm_right2", "arm_right", [-4.0, 14.0, 0.0]),
            ("leg_left", "body", [2.0, 8.0, 0.0]),
            ("leg_right", "body", [-2.0, 8.0, 0.0]),
            ("tail", "body", [0.0, 8.0, 3.0]),
        ]);
        let r = Rig::of(&m);
        assert_eq!(r.plan, Plan::Biped);
        assert_eq!(r.arms.len(), 2, "arm_left2 is the elbow, not a second arm");
        assert!(r.arms.iter().all(|&(_, _, lower)| lower.is_some()));
        let p = r.pose(false, 1.0);
        for b in ["torso", "head", "jaw", "arm_left", "arm_left2", "leg_left", "tail"] {
            assert!(moved(&m, &p, b), "{b} should move");
        }
        assert!(p.fwd > 0.0 && p.lean > 0.0);
    }

    #[test]
    fn palkia_style_names() {
        let m = model(&[
            ("torso", "", [0.0; 3]),
            ("arms", "torso", [0.0; 3]),
            ("left_arm", "arms", [3.0, 18.0, 0.0]),
            ("shoulder", "left_arm", [3.0, 18.0, 0.0]),
            ("left_arm_armor", "shoulder", [3.0, 18.0, 0.0]),
            ("arm", "shoulder", [3.0, 18.0, 0.0]),
            ("forearm", "arm", [3.0, 14.0, 0.0]),
            ("right_arm", "arms", [-3.0, 18.0, 0.0]),
        ]);
        let r = Rig::of(&m);
        assert_eq!(r.arms.len(), 2);
        let left = r.arms.iter().find(|a| a.1 > 0.0).unwrap();
        assert_eq!(m.bones[left.2.unwrap()].key, "forearm");
    }

    #[test]
    fn plans_from_names() {
        let quad = model(&[
            ("body", "", [0.0; 3]),
            ("head", "body", [0.0, 10.0, -8.0]),
            ("leg_front_left", "body", [2.0, 6.0, -5.0]),
            ("leg_front_right", "body", [-2.0, 6.0, -5.0]),
            ("leg_back_left", "body", [2.0, 6.0, 5.0]),
            ("leg_back_right", "body", [-2.0, 6.0, 5.0]),
        ]);
        let r = Rig::of(&quad);
        assert_eq!(r.plan, Plan::Quadruped);
        assert_eq!((r.front_legs.len(), r.back_legs.len()), (2, 2));
        let short = model(&[("body", "", [0.0; 3]), ("lleg", "body", [2.0, 6.0, 0.0]), ("rleg", "body", [-2.0, 6.0, 0.0])]);
        assert_eq!(Rig::of(&short).plan, Plan::Biped);
        let snake = model(&[
            ("body", "", [0.0; 3]),
            ("neck1", "body", [0.0; 3]),
            ("neck2", "neck1", [0.0; 3]),
            ("head", "neck2", [0.0; 3]),
            ("tail", "body", [0.0; 3]),
        ]);
        let r = Rig::of(&snake);
        assert_eq!((r.plan, r.necks.len()), (Plan::Serpent, 2));
        let fish = model(&[("body", "", [0.0; 3]), ("tail", "body", [0.0; 3]), ("fin_left", "body", [1.0, 0.0, 0.0])]);
        assert_eq!(Rig::of(&fish).plan, Plan::Fish);
        let bird = model(&[("body", "", [0.0; 3]), ("wing_left", "body", [2.0; 3]), ("wing_right", "body", [-2.0, 2.0, 2.0])]);
        assert_eq!(Rig::of(&bird).plan, Plan::Flyer);
        let ball = model(&[("body", "", [0.0; 3])]);
        let r = Rig::of(&ball);
        assert_eq!(r.plan, Plan::Blob);
        // Even with nothing to bend, the whole body still moves.
        let p = r.pose(false, 1.0);
        assert!(p.fwd > 0.0 && p.squash != 0.0);
    }

    #[test]
    fn amount_shape() {
        // Physical: coils (negative) through the wind-up, full strike just after, back to 0.
        assert!(amount(false, 0.2, 0.4) < 0.0);
        assert!((amount(false, 0.55, 0.4) - 1.0).abs() < 1e-3);
        assert!(amount(false, secs(false, 0.4) - 0.001, 0.4).abs() < 0.01);
        // Special: up, held, down.
        assert!((amount(true, 0.5, 0.6) - 1.0).abs() < 1e-3);
        assert!(amount(true, secs(true, 0.6) - 0.001, 0.6).abs() < 0.01);
    }
}
