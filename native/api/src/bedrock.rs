//! Bedrock (Blockbench "Bedrock Entity") geometry and keyframe animations, as Cobblemon
//! ships them: `*.geo.json` and `*.animation.json`.
//!
//! Coordinates: the files use Bedrock's model space, where x points to the model's
//! right as Blockbench shows it mirrored. Like Blockbench, we first flip x ("viewport
//! space": right-handed, +y up, the model faces -z, its right side is +x). Then we turn
//! it to Horizon's mob frame: +z the way the mob faces, +x its left, +y up, in blocks.
//! Rotations follow Blockbench too: a file's `[rx, ry, rz]` becomes `(-rx, -ry, rz)`,
//! applied Z·Y·X. Animations add to the bone's rotation, keyframe x and y negated the same way.

use glam::{Mat4, Vec3};
use serde_json::Value;
use std::collections::HashMap;

/// One box of a bone, in viewport space (pixels).
#[derive(Clone, Debug)]
pub struct Cube {
    pub min: Vec3,
    pub max: Vec3,
    /// Box UV origin and the box's size before `inflate`, for the UV layout.
    pub uv: [f32; 2],
    pub size: Vec3,
    pub mirror: bool,
    /// The cube's own rotation about its pivot, already converted.
    pub pivot: Vec3,
    pub rotation: Vec3,
    /// How far each face is pushed out along its normal, in pixels, beyond
    /// [`FACE_PUSH`]: north, south, east, west, up, down. Set by
    /// [`Model::separate_coplanar`] so faces lying flush on another cube's face draw
    /// in front of it instead of fighting it.
    pub bias: [f32; 6],
}

/// Every face is pushed out this far (pixels). Horizon's entity pass draws both
/// sides of a quad (no back-face culling), so a zero-thickness cube's two faces would
/// otherwise share one plane and z-fight. This separates them by twice this much.
pub const FACE_PUSH: f32 = 0.01;
/// The extra push for each face lying flush on an earlier cube's face (pixels).
pub const LAYER_PUSH: f32 = 0.02;

const NORMALS: [Vec3; 6] = [Vec3::NEG_Z, Vec3::Z, Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y];

#[derive(Clone, Debug)]
pub struct Bone {
    pub name: String,
    /// `name` lowercased: Bedrock matches bone names case-insensitively.
    pub key: String,
    pub parent: Option<usize>,
    pub pivot: Vec3,
    pub rotation: Vec3,
    pub cubes: Vec<Cube>,
}

#[derive(Clone, Debug)]
pub struct Model {
    pub texture: [f32; 2],
    pub bones: Vec<Bone>,
    /// Bone indices with every parent before its children.
    order: Vec<usize>,
}

fn num(v: &Value) -> f32 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0) as f32,
        // Molang is not evaluated yet: a constant string reads as its number, else 0.
        Value::String(s) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn vec3(v: Option<&Value>) -> Option<Vec3> {
    let a = v?.as_array()?;
    let g = |i: usize| a.get(i).map(num).unwrap_or(0.0);
    Some(Vec3::new(g(0), g(1), g(2)))
}

/// One animation value: a number, or a Molang expression of the animation time.
#[derive(Clone, Debug)]
pub enum Term {
    N(f32),
    E(crate::molang::Expr),
}

impl Term {
    fn parse(v: &Value) -> Self {
        match v {
            Value::Number(n) => Self::N(n.as_f64().unwrap_or(0.0) as f32),
            Value::String(s) => match s.trim().parse::<f32>() {
                Ok(n) if n.is_finite() => Self::N(n),
                Ok(_) => Self::N(0.0),
                Err(_) => crate::molang::parse(s).map(Self::E).unwrap_or(Self::N(0.0)),
            },
            _ => Self::N(0.0),
        }
    }
    fn at(&self, t: f32) -> f32 {
        match self {
            Self::N(n) => *n,
            Self::E(e) => e.eval(t),
        }
    }
}

/// An animation vector whose components may be Molang.
#[derive(Clone, Debug)]
pub struct V3([Term; 3]);

impl V3 {
    fn parse(v: Option<&Value>) -> Option<Self> {
        match v? {
            Value::Array(a) => {
                let g = |i: usize| a.get(i).map(Term::parse).unwrap_or(Term::N(0.0));
                Some(Self([g(0), g(1), g(2)]))
            }
            // A single number or expression applies to all three axes.
            s @ (Value::Number(_) | Value::String(_)) => {
                let t = Term::parse(s);
                Some(Self([t.clone(), t.clone(), t]))
            }
            _ => None,
        }
    }
    fn at(&self, t: f32) -> Vec3 {
        Vec3::new(self.0[0].at(t), self.0[1].at(t), self.0[2].at(t))
    }
}

/// A file position (pivot) in viewport space.
fn pos_in(v: Vec3) -> Vec3 {
    Vec3::new(-v.x, v.y, v.z)
}

/// A file rotation in viewport space.
fn rot_in(v: Vec3) -> Vec3 {
    Vec3::new(-v.x, -v.y, v.z)
}

/// Z·Y·X rotation from degrees.
pub fn euler(deg: Vec3) -> Mat4 {
    Mat4::from_rotation_z(deg.z.to_radians())
        * Mat4::from_rotation_y(deg.y.to_radians())
        * Mat4::from_rotation_x(deg.x.to_radians())
}

impl Model {
    pub fn parse(text: &str) -> Result<Self, String> {
        let json: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let geo = json
            .get("minecraft:geometry")
            .and_then(|g| g.get(0))
            .ok_or("no minecraft:geometry")?;
        let desc = &geo["description"];
        let texture = [
            desc["texture_width"].as_f64().unwrap_or(64.0) as f32,
            desc["texture_height"].as_f64().unwrap_or(64.0) as f32,
        ];
        let list = geo["bones"].as_array().ok_or("no bones")?;
        let mut index = HashMap::new();
        for (i, b) in list.iter().enumerate() {
            index.insert(b["name"].as_str().unwrap_or_default().to_ascii_lowercase(), i);
        }
        let mut bones = Vec::with_capacity(list.len());
        for b in list {
            let mut cubes = Vec::new();
            for c in b["cubes"].as_array().into_iter().flatten() {
                let origin = vec3(c.get("origin")).unwrap_or(Vec3::ZERO);
                let size = vec3(c.get("size")).unwrap_or(Vec3::ZERO);
                let inflate = c.get("inflate").map(num).unwrap_or(0.0);
                // Viewport space: x flipped, so the box spans -(ox+sx)..-ox.
                let min = Vec3::new(-(origin.x + size.x), origin.y, origin.z) - Vec3::splat(inflate);
                let max = Vec3::new(-origin.x, origin.y + size.y, origin.z + size.z) + Vec3::splat(inflate);
                let uv = c
                    .get("uv")
                    .and_then(Value::as_array)
                    .map(|a| [a.first().map(num).unwrap_or(0.0), a.get(1).map(num).unwrap_or(0.0)])
                    .unwrap_or([0.0, 0.0]);
                cubes.push(Cube {
                    min,
                    max,
                    uv,
                    size,
                    mirror: c.get("mirror").and_then(Value::as_bool).unwrap_or(false),
                    pivot: pos_in(vec3(c.get("pivot")).unwrap_or(Vec3::ZERO)),
                    rotation: rot_in(vec3(c.get("rotation")).unwrap_or(Vec3::ZERO)),
                    bias: [0.0; 6],
                });
            }
            bones.push(Bone {
                name: b["name"].as_str().unwrap_or_default().to_owned(),
                key: b["name"].as_str().unwrap_or_default().to_ascii_lowercase(),
                parent: b.get("parent").and_then(Value::as_str).and_then(|p| index.get(&p.to_ascii_lowercase()).copied()),
                pivot: pos_in(vec3(b.get("pivot")).unwrap_or(Vec3::ZERO)),
                rotation: rot_in(vec3(b.get("rotation")).unwrap_or(Vec3::ZERO)),
                cubes,
            });
        }
        // Parents first.
        let mut order = Vec::with_capacity(bones.len());
        let mut placed = vec![false; bones.len()];
        while order.len() < bones.len() {
            let before = order.len();
            for i in 0..bones.len() {
                if !placed[i] && bones[i].parent.is_none_or(|p| placed[p]) {
                    placed[i] = true;
                    order.push(i);
                }
            }
            if order.len() == before {
                return Err("bone parents form a loop".into());
            }
        }
        let mut model = Self { texture, bones, order };
        model.separate_coplanar();
        Ok(model)
    }

    /// Finds faces that lie flush on an earlier cube's face, facing the same way and
    /// overlapping it (in the rest pose), and pushes each one out a little further
    /// per face it covers, so the later cube (usually a marking or detail) draws
    /// on top. Returns how many faces were pushed.
    pub fn separate_coplanar(&mut self) -> usize {
        let pose = self.pose(None, 0.0);
        // Every face in the rest pose, in pixels: (bone, cube, face, corners).
        let mut all = Vec::new();
        for (bi, bone) in self.bones.iter().enumerate() {
            let m = pose[bi] * Mat4::from_translation(-bone.pivot);
            for (ci, c) in bone.cubes.iter().enumerate() {
                let m = m * cube_matrix(c);
                for (fi, corners, _) in face_list(c) {
                    all.push((bi, ci, fi, corners.map(|p| m.transform_point3(p))));
                }
            }
        }
        // Pairs that (nearly) share a plane: (behind, in front). The face already in
        // front stays in front; for an exact tie the later cube (a marking) wins.
        let mut over = Vec::new();
        for j in 0..all.len() {
            for i in 0..j {
                let ((bi, ci, ..), (bj, cj, ..)) = (&all[i], &all[j]);
                if (bi, ci) == (bj, cj) {
                    continue;
                }
                if let Some(ahead) = same_facing_overlap(&all[i].3, &all[j].3) {
                    over.push(if ahead >= 0.0 { (i, j) } else { (j, i) });
                }
            }
        }
        // Each face's layer: one above every face it covers.
        let mut layer = vec![0u32; all.len()];
        for _ in 0..8 {
            let mut changed = false;
            for &(below, above) in &over {
                if layer[above] <= layer[below] {
                    layer[above] = layer[below] + 1;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let mut pushed = 0;
        for (k, (b, c, f, _)) in all.iter().enumerate() {
            if layer[k] > 0 {
                self.bones[*b].cubes[*c].bias[*f] = LAYER_PUSH * layer[k] as f32;
                pushed += 1;
            }
        }
        pushed
    }

    pub fn cube_count(&self) -> usize {
        self.bones.iter().map(|b| b.cubes.len()).sum()
    }

    /// Each bone's matrix (viewport space, pixels) for `anim` at `time` seconds.
    pub fn pose(&self, anim: Option<&Animation>, time: f32) -> Vec<Mat4> {
        self.pose_with(anim, time, &[], 0.0)
    }

    /// A walk cycle made up for a model that has no walk animation (Cobblemon builds many of its
    /// walks in code, not in the files we import): `(bone, swing in file degrees)` for the top
    /// bone of each leg, alternating sides (diagonal pairs for four legs), arms against the legs,
    /// and a tail sway. A model with no legs or arms waddles its body instead. Empty if nothing fits.
    pub fn gait(&self) -> Vec<(usize, Vec3)> {
        let is = |k: &str, part: &str, group: &str| k.contains(part) && k != group && !k.contains(group) && !k.contains("armor");
        let leg = |k: &str| is(k, "leg", "legs");
        let arm = |k: &str| is(k, "arm", "arms");
        let tail = |k: &str| k.starts_with("tail");
        let top = |i: usize, f: &dyn Fn(&str) -> bool| {
            let mut p = self.bones[i].parent;
            while let Some(j) = p {
                if f(&self.bones[j].key) {
                    return false;
                }
                p = self.bones[j].parent;
            }
            true
        };
        let side = |k: &str| if k.contains("left") { Some(1.0) } else if k.contains("right") { Some(-1.0) } else { None };
        let back = |k: &str| k.contains("back") || k.contains("hind") || k.contains("rear");
        let mut out = Vec::new();
        let (mut legs, mut arms) = (0, 0);
        for (i, b) in self.bones.iter().enumerate() {
            let k = b.key.as_str();
            if leg(k) && top(i, &leg) {
                if let Some(s) = side(k) {
                    let s = if back(k) { -s } else { s };
                    out.push((i, Vec3::new(28.0 * s, 0.0, 0.0)));
                    legs += 1;
                }
            } else if arm(k) && top(i, &arm) {
                if let Some(s) = side(k) {
                    out.push((i, Vec3::new(-18.0 * s, 0.0, 0.0)));
                    arms += 1;
                }
            } else if tail(k) && top(i, &tail) {
                out.push((i, Vec3::new(0.0, 12.0, 0.0)));
            }
        }
        if legs + arms == 0 {
            if let Some(i) = self.bones.iter().position(|b| b.key == "body" || b.key == "torso") {
                out.push((i, Vec3::new(0.0, 0.0, 6.0)));
            }
        }
        out
    }

    /// `pose`, plus `extra` (as from `gait`) swung by `swing` (-1 to 1) on top of the animation.
    pub fn pose_with(&self, anim: Option<&Animation>, time: f32, extra: &[(usize, Vec3)], swing: f32) -> Vec<Mat4> {
        let mut world = vec![Mat4::IDENTITY; self.bones.len()];
        for &i in &self.order {
            let bone = &self.bones[i];
            let (mut rot, mut pos, mut scale) = (bone.rotation, Vec3::ZERO, Vec3::ONE);
            for (_, deg) in extra.iter().filter(|(j, _)| *j == i) {
                rot += rot_in(*deg * swing);
            }
            if let Some(track) = anim.and_then(|a| a.bones.get(&bone.key)) {
                let t = anim.map(|a| a.time(time)).unwrap_or(0.0);
                if let Some(r) = &track.rotation {
                    rot += rot_in(r.sample(t));
                }
                if let Some(p) = &track.position {
                    pos = pos_in(p.sample(t));
                }
                if let Some(s) = &track.scale {
                    scale = s.sample(t);
                }
            }
            let (parent, parent_pivot) = match bone.parent {
                Some(p) => (world[p], self.bones[p].pivot),
                None => (Mat4::IDENTITY, Vec3::ZERO),
            };
            world[i] = parent
                * Mat4::from_translation(bone.pivot - parent_pivot + pos)
                * euler(rot)
                * Mat4::from_scale(scale);
        }
        world
    }

    /// Every face of every cube, posed: `(corners, uv)` as `native_draw_quad` takes
    /// them, corners in Horizon's mob frame (blocks, scaled by `scale`), UVs in the
    /// sprite's 0..16 units. Corners run top-left, bottom-left, bottom-right,
    /// top-right as seen from outside.
    pub fn emit(&self, pose: &[Mat4], scale: f32, quad: impl FnMut([[f32; 3]; 4], [f32; 4])) {
        self.emit_under(pose, Self::to_mob(scale), quad);
    }

    /// Viewport space → the mob frame: half a turn about y, pixels to blocks.
    pub fn to_mob(scale: f32) -> Mat4 {
        Mat4::from_scale(Vec3::splat(scale / 16.0)) * Mat4::from_rotation_y(std::f32::consts::PI)
    }

    /// `emit` under any matrix to the mob frame: a hat rides on another model's bone this way.
    pub fn emit_under(&self, pose: &[Mat4], to_mob: Mat4, mut quad: impl FnMut([[f32; 3]; 4], [f32; 4])) {
        let [tw, th] = self.texture;
        for (bi, bone) in self.bones.iter().enumerate() {
            let bone_m = to_mob * pose[bi] * Mat4::from_translation(-bone.pivot);
            for c in &bone.cubes {
                let m = bone_m * cube_matrix(c);
                for (_, corners, mut uv) in face_list(c) {
                    if c.mirror {
                        uv.swap(0, 2);
                    }
                    let at = corners.map(|p| m.transform_point3(p).to_array());
                    quad(at, [uv[0] * 16.0 / tw, uv[1] * 16.0 / th, uv[2] * 16.0 / tw, uv[3] * 16.0 / th]);
                }
            }
        }
    }
}

/// A cube's own rotation about its pivot.
fn cube_matrix(c: &Cube) -> Mat4 {
    if c.rotation == Vec3::ZERO {
        Mat4::IDENTITY
    } else {
        Mat4::from_translation(c.pivot) * euler(c.rotation) * Mat4::from_translation(-c.pivot)
    }
}

/// The faces with area, each as its index (north, south, east, west, up, down), corners
/// (viewport space) pushed out along the face's normal by [`FACE_PUSH`] plus its bias,
/// and UV `[u0, v0, u1, v1]` in pixels.
fn face_list(c: &Cube) -> Vec<(usize, [Vec3; 4], [f32; 4])> {
    faces(c)
        .into_iter()
        .map(|(i, corners, uv)| {
            let push = NORMALS[i] * (FACE_PUSH + c.bias[i]);
            (i, corners.map(|p| p + push), uv)
        })
        .collect()
}

/// Faces closer than this (pixels) to another's plane count as flush with it.
const FLUSH: f32 = 0.05;

/// When quad `b` lies (within [`FLUSH`]) in quad `a`'s plane, faces the same way and
/// overlaps it with some area: how far `b` sits in front of `a` (negative: behind).
fn same_facing_overlap(a: &[Vec3; 4], b: &[Vec3; 4]) -> Option<f32> {
    let (ua, va) = (a[3] - a[0], a[1] - a[0]);
    let (lu, lv) = (ua.length(), va.length());
    let n = ua.cross(va);
    let (na, nb) = (n.normalize_or_zero(), (b[3] - b[0]).cross(b[1] - b[0]).normalize_or_zero());
    if lu < 1e-4 || lv < 1e-4 || na == Vec3::ZERO || na.dot(nb) < 0.999 {
        return None;
    }
    // The winding gives the face's outward normal: TL, BL, BR, TR seen from outside.
    let na = -na;
    let ahead = b.iter().map(|p| (*p - a[0]).dot(na)).sum::<f32>() / 4.0;
    if b.iter().any(|p| (*p - a[0]).dot(na).abs() > FLUSH) {
        return None;
    }
    // Overlap of b's extent with a's rectangle, in a's own axes.
    let (eu, ev) = (ua / lu, va / lv);
    let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
    for p in b {
        let d = *p - a[0];
        let (x, y) = (d.dot(eu), d.dot(ev));
        lo = [lo[0].min(x), lo[1].min(y)];
        hi = [hi[0].max(x), hi[1].max(y)];
    }
    let ox = hi[0].min(lu) - lo[0].max(0.0);
    let oy = hi[1].min(lv) - lo[1].max(0.0);
    (ox > 1e-3 && oy > 1e-3).then_some(ahead)
}

fn faces(c: &Cube) -> Vec<(usize, [Vec3; 4], [f32; 4])> {
    let (n, x) = (c.min, c.max);
    let (w, h, d) = (c.size.x, c.size.y, c.size.z);
    let [u, v] = c.uv;
    let p = Vec3::new;
    // Box UV regions (Blockbench/vanilla layout). Mirroring swaps the two sides.
    let mut east = [u, v + d, u + d, v + d + h];
    let mut west = [u + d + w, v + d, u + d + w + d, v + d + h];
    if c.mirror {
        std::mem::swap(&mut east, &mut west);
    }
    let north = [u + d, v + d, u + d + w, v + d + h];
    let south = [u + d + w + d, v + d, u + d + w + d + w, v + d + h];
    let up = [u + d, v, u + d + w, v + d];
    let down = [u + d + w, v, u + d + w + w, v + d];
    let mut out = Vec::with_capacity(6);
    if w > 0.0 && h > 0.0 {
        out.push((0, [p(x.x, x.y, n.z), p(x.x, n.y, n.z), p(n.x, n.y, n.z), p(n.x, x.y, n.z)], north));
        out.push((1, [p(n.x, x.y, x.z), p(n.x, n.y, x.z), p(x.x, n.y, x.z), p(x.x, x.y, x.z)], south));
    }
    if d > 0.0 && h > 0.0 {
        out.push((2, [p(x.x, x.y, x.z), p(x.x, n.y, x.z), p(x.x, n.y, n.z), p(x.x, x.y, n.z)], east));
        out.push((3, [p(n.x, x.y, n.z), p(n.x, n.y, n.z), p(n.x, n.y, x.z), p(n.x, x.y, x.z)], west));
    }
    if w > 0.0 && d > 0.0 {
        out.push((4, [p(x.x, x.y, x.z), p(x.x, x.y, n.z), p(n.x, x.y, n.z), p(n.x, x.y, x.z)], up));
        out.push((5, [p(x.x, n.y, n.z), p(x.x, n.y, x.z), p(n.x, n.y, x.z), p(n.x, n.y, n.z)], down));
    }
    out
}

#[derive(Clone, Debug)]
pub struct Key {
    time: f32,
    pre: V3,
    post: V3,
    smooth: bool,
}

#[derive(Clone, Debug)]
pub enum Channel {
    Constant(V3),
    Keys(Vec<Key>),
}

impl Channel {
    fn parse(v: &Value) -> Option<Self> {
        if !v.is_object() {
            return V3::parse(Some(v)).map(Self::Constant);
        }
        let map = v.as_object()?;
        let mut keys: Vec<Key> = map
            .iter()
            .filter_map(|(t, k)| {
                let time: f32 = t.parse().ok()?;
                let (pre, post, smooth) = if let Some(o) = k.as_object() {
                    let post = V3::parse(o.get("post").or(o.get("vector")))?;
                    let pre = V3::parse(o.get("pre")).unwrap_or_else(|| post.clone());
                    (pre, post, o.get("lerp_mode").and_then(Value::as_str) == Some("catmullrom"))
                } else {
                    let v = V3::parse(Some(k))?;
                    (v.clone(), v, false)
                };
                Some(Key { time, pre, post, smooth })
            })
            .collect();
        keys.sort_by(|a, b| a.time.total_cmp(&b.time));
        (!keys.is_empty()).then_some(Self::Keys(keys))
    }

    fn last_time(&self) -> f32 {
        match self {
            Self::Constant(_) => 0.0,
            Self::Keys(k) => k.last().map(|k| k.time).unwrap_or(0.0),
        }
    }

    /// The channel's value at animation time `t` (seconds); Molang sees the same `t`.
    pub fn sample(&self, t: f32) -> Vec3 {
        let keys = match self {
            Self::Constant(v) => return v.at(t),
            Self::Keys(k) => k,
        };
        let first = &keys[0];
        if t <= first.time || keys.len() == 1 {
            return if t < first.time { first.pre.at(t) } else { first.post.at(t) };
        }
        let last = &keys[keys.len() - 1];
        if t >= last.time {
            return last.post.at(t);
        }
        let i = keys.iter().rposition(|k| k.time <= t).unwrap_or(0);
        let (a, b) = (&keys[i], &keys[i + 1]);
        let span = (b.time - a.time).max(1e-6);
        let s = (t - a.time) / span;
        if a.smooth || b.smooth {
            let p0 = if i > 0 { keys[i - 1].post.at(t) } else { a.post.at(t) };
            let p3 = keys.get(i + 2).map(|k| k.pre.at(t)).unwrap_or(b.pre.at(t));
            catmull_rom(p0, a.post.at(t), b.pre.at(t), p3, s)
        } else {
            a.post.at(t).lerp(b.pre.at(t), s)
        }
    }
}

fn catmull_rom(p0: Vec3, p1: Vec3, p2: Vec3, p3: Vec3, t: f32) -> Vec3 {
    let (t2, t3) = (t * t, t * t * t);
    0.5 * ((2.0 * p1) + (-p0 + p2) * t + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2 + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t3)
}

#[derive(Clone, Debug, Default)]
pub struct Track {
    pub rotation: Option<Channel>,
    pub position: Option<Channel>,
    pub scale: Option<Channel>,
}

#[derive(Clone, Debug)]
pub struct Animation {
    pub length: f32,
    pub looping: bool,
    pub bones: HashMap<String, Track>,
}

impl Animation {
    /// Every animation in a `*.animation.json`, by its short name (`ground_idle` for
    /// `animation.zeraora.ground_idle`).
    pub fn parse_all(text: &str) -> Result<HashMap<String, Self>, String> {
        let json: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let list = json["animations"].as_object().ok_or("no animations")?;
        let mut out = HashMap::new();
        for (name, a) in list {
            let short = name.rsplit('.').next().unwrap_or(name).to_owned();
            let mut bones = HashMap::new();
            for (bone, ch) in a["bones"].as_object().into_iter().flatten() {
                // Bedrock matches bone names case-insensitively.
                bones.insert(
                    bone.to_ascii_lowercase(),
                    Track {
                        rotation: ch.get("rotation").and_then(Channel::parse),
                        position: ch.get("position").and_then(Channel::parse),
                        scale: ch.get("scale").and_then(Channel::parse),
                    },
                );
            }
            // Without `animation_length`, Bedrock uses the last keyframe; an animation
            // with no keyframes at all (pure Molang) just runs on the clock.
            let length = a["animation_length"].as_f64().map(|n| n as f32).unwrap_or_else(|| {
                bones
                    .values()
                    .flat_map(|t: &Track| [&t.rotation, &t.position, &t.scale])
                    .flatten()
                    .map(Channel::last_time)
                    .fold(0.0, f32::max)
            });
            out.insert(
                short,
                Self {
                    length,
                    looping: a["loop"].as_bool().unwrap_or(false) || a["loop"].as_str() == Some("true"),
                    bones,
                },
            );
        }
        Ok(out)
    }

    pub fn time(&self, t: f32) -> f32 {
        if self.length <= 0.0 {
            t
        } else if self.looping {
            t.rem_euclid(self.length)
        } else {
            t.min(self.length)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Zeraora's model and animations, when quick.ps1 points at the content folder.
    fn zeraora() -> Option<(String, String)> {
        let dir = std::path::PathBuf::from(std::env::var("CAPI_TEST_CONTENT_DIR").ok()?).join("cobblemon-models");
        let read = |ext: &str| std::fs::read_to_string(dir.join(format!("zeraora.{ext}.json"))).unwrap();
        Some((read("geo"), read("animation")))
    }

    #[test]
    fn zeraora_parses() {
        let Some((geo, anim)) = zeraora() else { return };
        let m = Model::parse(&geo).unwrap();
        assert_eq!(m.bones.len(), 83);
        assert_eq!(m.cube_count(), 118);
        assert_eq!(m.texture, [128.0, 128.0]);
        let a = Animation::parse_all(&anim).unwrap();
        assert!(a.contains_key("ground_idle") && a.contains_key("ground_walk"));
        assert!(a["ground_walk"].looping);
    }

    #[test]
    fn zeraora_stands_on_its_feet_facing_forward() {
        let Some((geo, _)) = zeraora() else { return };
        let m = Model::parse(&geo).unwrap();
        let pose = m.pose(None, 0.0);
        let mut lo = Vec3::splat(f32::MAX);
        let mut hi = Vec3::splat(f32::MIN);
        let mut quads = 0;
        m.emit(&pose, 1.0, |c, uv| {
            quads += 1;
            for p in c {
                lo = lo.min(Vec3::from(p));
                hi = hi.max(Vec3::from(p));
            }
            assert!(uv.iter().all(|t| (-0.01..=16.01).contains(t)), "{uv:?}");
        });
        assert!(quads > 300, "{quads}");
        // Feet near y = 0, height between 1 and 3 blocks unscaled.
        assert!(lo.y > -0.3 && lo.y < 0.3, "{lo}");
        assert!(hi.y > 1.0 && hi.y < 3.0, "{hi}");
        println!("bounds {lo} .. {hi}, {quads} quads");
    }

    #[test]
    fn keys_interpolate() {
        let c = Channel::parse(&serde_json::json!({"0.0": [0, 0, 0], "1.0": [10, 20, 30]})).unwrap();
        assert_eq!(c.sample(0.5), Vec3::new(5.0, 10.0, 15.0));
        assert_eq!(c.sample(2.0), Vec3::new(10.0, 20.0, 30.0));
    }

    #[test]
    fn walk_moves_bones() {
        let Some((geo, anim)) = zeraora() else { return };
        let m = Model::parse(&geo).unwrap();
        let a = Animation::parse_all(&anim).unwrap();
        let p0 = m.pose(Some(&a["ground_walk"]), 0.0);
        let p1 = m.pose(Some(&a["ground_walk"]), 0.25);
        assert!(p0.iter().zip(&p1).any(|(x, y)| !x.abs_diff_eq(*y, 1e-4)));
    }
}
