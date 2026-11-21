//! The client half of cosmetics: hats on the head bone, billboard particles and the wardrobe.
//!
//! Particles are camera-facing quads drawn with the body in `player_submit`, not move effects: the
//! post pass has 16 effect slots shared with every move on screen, and always-on auras would push
//! combat effects out. Each player gets a small budget that falls with distance and on Low FX.

use super::*;
use crate::bedrock::Model;
use glam::{DVec3, Mat3, Mat4, Vec3};
use horizon_client::mod_api::BlockEntityDraw;
use std::collections::VecDeque;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Worn {
    pub hat: Option<String>,
    pub aura: Option<String>,
    pub trail: Option<String>,
}

impl Worn {
    fn empty(&self) -> bool {
        self.hat.is_none() && self.aura.is_none() && self.trail.is_none()
    }

    fn set(&mut self, slot: Slot, id: Option<String>) {
        match slot {
            Slot::Hat => self.hat = id,
            Slot::Aura => self.aura = id,
            Slot::Trail => self.trail = id,
        }
    }

    fn get(&self, slot: Slot) -> Option<&str> {
        match slot {
            Slot::Hat => self.hat.as_deref(),
            Slot::Aura => self.aura.as_deref(),
            Slot::Trail => self.trail.as_deref(),
        }
    }
}

struct Point {
    at: DVec3,
    born: Instant,
    seed: u32,
}

#[derive(Default)]
struct Trail {
    last: Option<DVec3>,
    points: VecDeque<Point>,
    count: u32,
}

struct View {
    shards: u32,
    worn: Worn,
    owned: HashSet<String>,
    tab: usize,
    sel: usize,
    /// Index into `filters` for this tab.
    filter: usize,
}

#[derive(Default)]
pub struct CosClient {
    worn: HashMap<String, Worn>,
    view: Option<View>,
    note: Option<(String, Instant)>,
    /// Other players' cosmetics are hidden on this client.
    hide_others: bool,
    /// The setting above has been read.
    read: bool,
    trails: HashMap<String, Trail>,
}

static CLIENT: LazyLock<Mutex<CosClient>> = LazyLock::new(Default::default);

const SETTING: &str = "cosmetics-others";
const TABS: [&str; 4] = ["Hats", "Auras", "Trails", "Shop"];

/// Rows shown per page, and how far Page Up/Down (or Z/C) jump.
const SHOWN: usize = 9;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Filter {
    All,
    Owned,
    /// One of the catalogue's hat sets.
    Set(usize),
}

/// The filters Q/R step through on a tab: sets only where there are hats.
fn filters(cat: &Catalog, tab: usize) -> Vec<Filter> {
    let mut out = vec![Filter::All];
    if tab != 3 {
        out.push(Filter::Owned);
    }
    if tab == 0 || tab == 3 {
        out.extend((0..cat.sets.len()).map(Filter::Set));
    }
    out
}

fn filter_name(cat: &Catalog, f: Filter) -> &str {
    match f {
        Filter::All => "All",
        Filter::Owned => "Owned",
        Filter::Set(i) => cat.sets.get(i).map_or("?", |s| s.name.as_str()),
    }
}

/// The rows of a wardrobe tab under a filter: owned first, then by rarity, a set in its own order.
/// The shop lists what can be bought. The one list the cursor, the try-on and Enter all read.
fn rows<'c>(cat: &'c Catalog, tab: usize, owned: &HashSet<String>, filter: usize) -> Vec<&'c Item> {
    let f = filters(cat, tab).get(filter).copied().unwrap_or(Filter::All);
    let in_tab = |i: &&Item| match tab {
        0 => i.slot == Slot::Hat,
        1 => i.slot == Slot::Aura,
        2 => i.slot == Slot::Trail,
        _ => i.capsule() && !owned.contains(&i.id),
    };
    let mut list: Vec<&Item> = match f {
        Filter::Set(n) => cat.sets.get(n).map_or(vec![], |s| s.items.iter().filter_map(|id| cat.item(id)).filter(in_tab).collect()),
        Filter::Owned => cat.items.iter().filter(in_tab).filter(|i| owned.contains(&i.id)).collect(),
        Filter::All => cat.items.iter().filter(in_tab).collect(),
    };
    // Stable: a set keeps its own order inside each owned/rarity group.
    list.sort_by_key(|i| (!owned.contains(&i.id), rank(i.rarity)));
    list
}

/// Where the cursor lands after a page jump: clamped, not wrapped.
fn page(sel: usize, n: usize, down: bool) -> usize {
    if down {
        (sel + SHOWN).min(n.saturating_sub(1))
    } else {
        sel.saturating_sub(SHOWN)
    }
}

/// A message for the cosmetics client; true when it was one.
pub fn heard(f: &[&str]) -> bool {
    use crate::play::onboard::menus::{self, Menu};
    let mut c = CLIENT.lock().unwrap();
    let part = |s: &str| (s != "-").then(|| s.to_string());
    let mut opened = false;
    match f {
        ["cosm", name, hat, aura, trail, ..] => {
            let worn = Worn { hat: part(hat), aura: part(aura), trail: part(trail) };
            if worn.trail.is_none() {
                c.trails.remove(*name);
            }
            c.worn.insert(name.to_string(), worn);
        }
        ["ward", "-", ..] => c.view = None,
        // A late refresh of a wardrobe this client just closed for another menu.
        ["ward", ..] if c.view.is_none() && !menus::server_may_open(Menu::Wardrobe) => {}
        ["ward", shards, hat, aura, trail, owned, ..] => {
            opened = c.view.is_none();
            let shop = SHOP_NEXT.swap(false, std::sync::atomic::Ordering::Relaxed);
            let (tab, sel, filter) = c.view.as_ref().map_or(if shop { (3, 0, 0) } else { (0, 0, 0) }, |v| (v.tab, v.sel, v.filter));
            c.view = Some(View {
                shards: shards.parse().unwrap_or(0),
                worn: Worn { hat: part(hat), aura: part(aura), trail: part(trail) },
                owned: owned.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect(),
                tab,
                sel,
                filter,
            });
        }
        ["wardnote", text, ..] => c.note = Some((text.to_string(), Instant::now())),
        _ => return false,
    }
    drop(c);
    if opened {
        menus::opening(Menu::Wardrobe);
    }
    true
}

pub fn forget() {
    let mut c = CLIENT.lock().unwrap();
    let (hide, read) = (c.hide_others, c.read);
    *c = CosClient { hide_others: hide, read, ..Default::default() };
}

/// The Tab menu's Shop row: the wardrobe the server opens next starts on the Shop tab.
static SHOP_NEXT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn open_on_shop() {
    SHOP_NEXT.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub fn is_open() -> bool {
    CLIENT.lock().unwrap().view.is_some()
}

/// Closes it (another menu is opening); true when it was open.
pub fn shut() -> bool {
    CLIENT.lock().unwrap().view.take().is_some()
}

/// What `name` shows on this client: their loadout, none when other players' are hidden, and for
/// this client's own player whatever the wardrobe is trying on.
pub fn worn_by(name: &str, local: bool) -> Option<Worn> {
    let c = CLIENT.lock().unwrap();
    if !local && c.hide_others {
        return None;
    }
    let mut w = c.worn.get(name).cloned().unwrap_or_default();
    if local {
        if let Some(v) = &c.view {
            let cat = catalog();
            if let Some(item) = rows(&cat, v.tab, &v.owned, v.filter).get(v.sel) {
                w.set(item.slot, Some(item.id.clone()));
            }
        }
    }
    (!w.empty()).then_some(w)
}

/// The local Pokémon as last drawn, for the wardrobe camera, in the body's frame (feet at the
/// origin, +z forward, blocks at its drawn size): body height and reach, the box around the head
/// with the hat on it and the box around the whole body, both from the posed geometry of this very
/// frame, the way the body faces, and when.
#[derive(Clone, Debug)]
pub struct LocalBody {
    pub height: f32,
    pub radius: f32,
    /// Centre and half-diagonal of the head and hat.
    pub head: Vec3,
    pub head_r: f32,
    /// The drawn body's box, min and max.
    pub body: (Vec3, Vec3),
    /// The body's quads (only while the wardrobe is open), to keep the camera's line of sight off them.
    pub quads: Vec<[Vec3; 4]>,
    pub yaw: f32,
    pub at: Instant,
}

static LOCAL_BODY: Mutex<Option<LocalBody>> = Mutex::new(None);

/// The box around `pts`.
fn bounds(pts: impl IntoIterator<Item = Vec3>) -> Option<(Vec3, Vec3)> {
    pts.into_iter().fold(None, |b, p| Some(b.map_or((p, p), |(lo, hi): (Vec3, Vec3)| (lo.min(p), hi.max(p)))))
}

/// The head and the hat on it, in the frame `to` maps the model's viewport space into: where the
/// hat anchor sits under this frame's pose (an 8-unit head under it, the anchor's own unit), plus
/// every corner of the hat worn there. Without an anchor, the posed pivot of the model's head bone;
/// without one, `None`.
pub fn head_box(model_id: &str, model: &Model, pose: &[Mat4], to: Mat4, hat: Option<&str>) -> Option<(Vec3, Vec3)> {
    let cat = catalog();
    let anchored = cat.anchors.get(model_id).and_then(|a| Some((a, model.bones.iter().position(|b| b.key == a.b)?)));
    if let Some((a, bi)) = anchored {
        let world = pose.get(bi)?;
        let m = to * *world * Mat4::from_translation(-model.bones[bi].pivot) * Mat4::from_translation(Vec3::from(a.p)) * Mat4::from_quat(a.rotation()) * Mat4::from_scale(Vec3::splat(a.s));
        let mut pts: Vec<Vec3> = [[-4.0, 0.0, -4.0], [4.0, 0.0, 4.0], [-4.0, -8.0, 4.0], [4.0, -8.0, -4.0]].iter().map(|c| m.transform_point3(Vec3::from(*c))).collect();
        if let Some(h) = hat.and_then(|id| cat.hats.get(id)) {
            h.emit_under(&h.pose(None, 0.0), m, |corners, _| pts.extend(corners.map(Vec3::from)));
        }
        return bounds(pts);
    }
    let bi = model.bones.iter().position(|b| b.key == "head").or_else(|| model.bones.iter().position(|b| b.key.contains("head")))?;
    let at = (to * pose.get(bi)?).transform_point3(Vec3::ZERO);
    let r = to.transform_vector3(Vec3::splat(4.0)).abs();
    Some((at - r, at + r))
}

/// Notes this window's own body as it's drawn: `pose` the frame's bone pose, `to` the model's
/// viewport space to the body's frame (draw scale and any size change), `hat` the one being worn
/// or tried on. The quads are gathered only while the wardrobe is open.
#[allow(clippy::too_many_arguments)]
pub fn note_local(model_id: &str, model: &Model, pose: &[Mat4], to: Mat4, height: f32, radius: f32, yaw: f32, hat: Option<&str>) {
    let mut quads = Vec::new();
    let open = is_open();
    model.emit_under(pose, to, |corners, _| {
        if open {
            quads.push(corners.map(Vec3::from));
        }
    });
    let body = if quads.is_empty() { None } else { bounds(quads.iter().flatten().copied()) };
    let body = body.unwrap_or((Vec3::new(-radius, 0.0, -radius), Vec3::new(radius, height, radius)));
    let (lo, hi) = head_box(model_id, model, pose, to, hat).unwrap_or_else(|| {
        let w = (height * 0.15).max(0.1);
        (Vec3::new(-w, height * 0.85 - w, -w), Vec3::new(w, height * 0.85 + w, w))
    });
    let head = (lo + hi) * 0.5;
    let head_r = ((hi - lo).length() * 0.5).max(0.12);
    *LOCAL_BODY.lock().unwrap() = Some(LocalBody { height, radius, head, head_r, body, quads, yaw, at: Instant::now() });
}

pub fn local_body() -> Option<LocalBody> {
    LOCAL_BODY.lock().unwrap().clone().filter(|b| b.at.elapsed().as_secs_f32() < 0.5)
}

/// The wardrobe camera's zoom (scroll wheel): 1 is the framing's own distance.
static ZOOM: Mutex<f32> = Mutex::new(1.0);

/// A wheel step while the wardrobe is open: up comes closer.
pub fn zoom_by(steps: f32) {
    let mut z = ZOOM.lock().unwrap();
    *z = (*z * 0.88f32.powf(steps)).clamp(0.45, 2.6);
}

pub fn zoom() -> f32 {
    *ZOOM.lock().unwrap()
}

pub fn reset_zoom() {
    *ZOOM.lock().unwrap() = 1.0;
}

/// While the wardrobe is open, which slot its camera frames: the row being tried on, else the tab's.
pub fn framing() -> Option<Slot> {
    let c = CLIENT.lock().unwrap();
    let v = c.view.as_ref()?;
    let cat = catalog();
    let slot = rows(&cat, v.tab, &v.owned, v.filter).get(v.sel).map(|i| i.slot);
    Some(slot.unwrap_or(match v.tab {
        0 => Slot::Hat,
        2 => Slot::Trail,
        _ => Slot::Aura,
    }))
}

/// A hat on the species' anchor bone, under the pose the body was just drawn with, so it follows
/// every animation, stand-in pose, dodge roll and Minimize.
pub fn hat(host: &dyn Host, draw: &mut BlockEntityDraw, id: &str, species: &str, model: &Model, pose: &[Mat4], scale: f32) {
    let cat = catalog();
    let (Some(item), Some(hat), Some(a)) = (cat.item(id), cat.hats.get(id), cat.anchors.get(species)) else { return };
    let (Some(info), Some(bi)) = (item.hat.as_ref(), model.bones.iter().position(|b| b.key == a.b)) else { return };
    let Some(world) = pose.get(bi) else { return };
    let m = Model::to_mob(scale)
        * *world
        * Mat4::from_translation(-model.bones[bi].pivot)
        * Mat4::from_translation(Vec3::from(a.p))
        * Mat4::from_quat(a.rotation())
        * Mat4::from_scale(Vec3::splat(a.s));
    hat.emit_under(&hat.pose(None, 0.0), m, |corners, uv| {
        host.draw_quad(draw, &info.sprite, corners, uv, 0xFF_FFFF);
    });
}

/// How many of `count` particles to draw at `dist` blocks.
pub fn budget(count: u32, dist: f32, low: bool) -> u32 {
    budget_for(count, dist, low, 1.0)
}

/// The same for a body of size `k` (`body_fit`): a bigger body gets more of them, up to twice as many
/// (32 at most), so a Moltres isn't ringed by the eight sparks a Pikachu has.
pub fn budget_for(count: u32, dist: f32, low: bool, k: f32) -> u32 {
    let share = if dist < 16.0 {
        1.0
    } else if dist < 32.0 {
        0.5
    } else {
        0.0
    };
    let share = if low { share * 0.5 } else { share };
    let more = k.sqrt().clamp(1.0, 2.0);
    ((count.min(16) as f32 * more * share).ceil() as u32).min(32)
}

/// A body's size for its cosmetics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    /// 1 for a Pikachu-sized body; scales particle size, count, and a trail's width and spacing.
    pub k: f32,
    /// The ring round the feet an aura circles (blocks).
    pub ring: f32,
    /// How high it reaches (blocks).
    pub height: f32,
}

/// The body a Pikachu-sized `k` of 1 stands for: sqrt(height x width) in blocks.
const FIT_REF: f32 = 0.8;

/// `height` is the drawn body's height and `reach` how far its model reaches out from the middle,
/// both in blocks and already times the model's scale and any size change (Minimize, Mega). A long
/// tail or wing reaches much further than the body is wide, so the ring is held within about the
/// body's height: a Steelix gets a ring round its coils, not one 14 blocks across.
pub fn body_fit(height: f32, reach: f32) -> Fit {
    let height = height.max(0.3);
    let ring = reach.clamp(0.3, height * 1.1 + 0.5);
    let k = ((height * ring * 2.0).sqrt() / FIT_REF).clamp(0.6, 4.5);
    Fit { k, ring, height }
}

impl Fit {
    /// A sprite's half width for a catalogue `size` (blocks): grows a little slower than the body,
    /// so a giant's sparks are big but don't turn into blobs.
    pub fn size(&self, size: f32) -> f32 {
        size * self.k.powf(0.65)
    }
}

// ---------------------------------------------------------------- legendary auras

/// How a legendary aura looks beyond its sprites (docs/COSMETICS.md "Legendary auras"): the
/// effects pass draws its ground ring, light sheath, pillars and flares by `theme`, and the coloured
/// light it throws, in `accent` over the sprites' tint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Legend {
    /// The type index the effects pass uses (`fx::type_index`: 0 normal .. 17 fairy), or 18 for starlight.
    pub theme: u32,
    /// 0xRRGGBB: the light's second colour (a fire's yellow-white core, a dark's crimson edge).
    pub accent: u32,
    /// The motes' sprite.
    pub mote: &'static str,
}

pub const STARLIGHT: u32 = 18;

/// The legendary auras. Any other aura of Legendary rarity gets a plain look in its own tint.
pub fn legend(id: &str) -> Option<Legend> {
    let l = |theme, accent, mote| Some(Legend { theme, accent, mote });
    match id {
        "aura.legend_fire" => l(1, 0xFF_E07A, "capipimon:entity/fx_ember"),
        "aura.legend_water" => l(2, 0x8F_F2FF, "capipimon:entity/fx_bubble"),
        "aura.storm_legend" => l(3, 0xF4_FBFF, "capipimon:entity/fx_spark"),
        "aura.plug_sparks" => l(3, 0xFF_F2B0, "capipimon:entity/fx_spark"),
        "aura.legend_grass" => l(4, 0xE8_FF9A, "capipimon:entity/fx_leaf"),
        "aura.legend_ice" => l(5, 0xFF_FFFF, "capipimon:entity/fx_snow"),
        "aura.legend_fighting" => l(6, 0xFF_C070, "capipimon:entity/fx_spark"),
        "aura.legend_poison" => l(7, 0xFF_5CE0, "capipimon:entity/fx_bubble"),
        "aura.legend_ground" => l(8, 0xFF_6A2A, "capipimon:entity/fx_ember"),
        "aura.tide_legend" => l(9, 0x6C_C8FF, "capipimon:entity/fx_droplet"),
        "aura.legend_psychic" => l(10, 0xB0_8CFF, "capipimon:entity/fx_spark"),
        "aura.legend_bug" => l(11, 0xFF_F07A, "capipimon:entity/fx_spark"),
        "aura.legend_rock" => l(12, 0xFF_C878, "capipimon:entity/fx_dust"),
        "aura.legend_ghost" => l(13, 0x40_FFD0, "capipimon:entity/fx_ember"),
        "aura.legend_dragon" => l(14, 0xFF_D25A, "capipimon:entity/fx_spark"),
        "aura.legend_dark" => l(15, 0xFF_2A3A, "capipimon:entity/fx_ember"),
        "aura.legend_steel" => l(16, 0x9C_E8FF, "capipimon:entity/fx_star"),
        "aura.legend_fairy" => l(17, 0xA0_F8FF, "capipimon:entity/fx_star"),
        "aura.legend_normal" => l(0, 0xFF_D870, "capipimon:entity/fx_star"),
        "aura.stars" => l(STARLIGHT, 0x9C_B8FF, "capipimon:entity/fx_star"),
        _ => None,
    }
}

/// A legendary aura as drawn this frame, for the effects pass and its light (`fx.rs`).
#[derive(Clone, Copy, Debug)]
pub struct LegendBody {
    pub feet: DVec3,
    pub fit: Fit,
    pub look: Legend,
    /// The aura's own tint, 0xRRGGBB.
    pub tint: u32,
    /// Stable per player: staggers the flares.
    pub seed: u32,
    /// From the camera, blocks.
    pub dist: f32,
    pub at: Instant,
}

static LEGENDS: LazyLock<Mutex<HashMap<String, LegendBody>>> = LazyLock::new(Default::default);

fn note_legend(name: &str, body: LegendBody) {
    LEGENDS.lock().unwrap().insert(name.to_string(), body);
}

/// The legendary auras drawn in the last moment (bodies drawn this frame or the one before), by
/// player. Nothing when other players' cosmetics are hidden and it isn't this client's own.
pub fn legends() -> Vec<(String, LegendBody)> {
    let mut l = LEGENDS.lock().unwrap();
    l.retain(|_, b| b.at.elapsed().as_secs_f32() < 0.25);
    l.iter().map(|(n, b)| (n.clone(), *b)).collect()
}

/// Seconds since this player's last flare, and how long between them (4.5-6.5 s, by seed).
pub fn flare(seed: u32, secs: f32) -> (f32, f32) {
    let period = 4.5 + 2.0 * ((seed % 101) as f32 / 101.0);
    let offset = (seed % 977) as f32 * 0.37;
    ((secs + offset) % period, period)
}

fn name_seed(name: &str) -> u32 {
    name.bytes().fold(2_166_136_261u32, |h, b| (h ^ u32::from(b)).wrapping_mul(16_777_619))
}

/// Where aura particle `i` of `n` is at `t` seconds, and how big (0..1), around a body of height
/// `h` and reach `r`, relative to the feet.
pub fn aura_at(motion: Motion, i: u32, t: f32, h: f32, r: f32) -> (Vec3, f32) {
    let tau = std::f32::consts::TAU;
    let phase = (i as f32 * 0.618_034).fract();
    let ring = |a: f32, rr: f32, y: f32| Vec3::new(a.cos() * rr, y, a.sin() * rr);
    match motion {
        Motion::Rise | Motion::Fall => {
            let u = (t * 0.4 + phase).fract();
            let y = if motion == Motion::Rise { u } else { 1.0 - u };
            let a = phase * tau + (t * 0.8 + phase * 9.0).sin() * 0.5;
            (ring(a, r * (0.85 + 0.25 * (phase * 37.0).sin()), y * h * 1.1), (u * std::f32::consts::PI).sin().sqrt())
        }
        Motion::Orbit => {
            let y = h * (0.3 + 0.6 * (phase * 3.0).fract()) + (t * 2.0 + phase * tau).sin() * 0.08 * h;
            (ring(t * 1.2 + phase * tau, r * 1.15, y), 1.0)
        }
        Motion::Flicker => {
            let step = t * 7.0 + phase * 13.0;
            let beat = step.floor();
            let rand = (beat * 12.9898 + i as f32 * 78.233).sin() * 43_758.547;
            let s = rand.fract().abs();
            let a = s * tau * 3.0;
            let lit = (s * 11.0).fract() > 0.45;
            (ring(a, r * (0.55 + 0.6 * s), h * (0.1 + 0.9 * (s * 7.0).fract())), if lit { 1.0 - step.fract() } else { 0.0 })
        }
    }
}

/// A camera-facing quad in the body's frame.
fn billboard(host: &dyn Host, draw: &mut BlockEntityDraw, sprite: &str, at: Vec3, right: Vec3, up: Vec3, half: f32, tint: u32) {
    let (r, u) = (right * half, up * half);
    let corners = [(at - r + u).to_array(), (at - r - u).to_array(), (at + r - u).to_array(), (at + r + u).to_array()];
    host.draw_quad(draw, sprite, corners, [0.0, 0.0, 16.0, 16.0], tint);
}

/// The aura around a body at the current draw origin; the trail too when `real` (not on the Double
/// Team copies: only the real body leaves one).
#[allow(clippy::too_many_arguments)]
/// `local`: this window's own body. The engine's entity list holds only the other players' bodies,
/// so its feet come from the camera's record of them (`play::local_feet`, third person).
pub fn particles(host: &dyn Host, draw: &mut BlockEntityDraw, w: &Worn, name: &str, height: f32, radius: f32, mob: &horizon_client::mod_api::NativeMobRender<'_>, real: bool, local: bool) {
    if w.aura.is_none() && (w.trail.is_none() || !real) {
        return;
    }
    let Some(cam) = host.camera() else { return };
    let feet = match host.client_entities().into_iter().find(|e| e.player_name.as_deref() == Some(name)) {
        Some(e) => {
            let lerp = |i: usize| e.previous_position[i] + (e.position[i] - e.previous_position[i]) * f64::from(mob.partial);
            DVec3::new(lerp(0), lerp(1), lerp(2))
        }
        None if local => match crate::play::local_feet() {
            Some(f) => DVec3::from_array(f),
            None => return,
        },
        None => return,
    };
    let to_cam = DVec3::from_array(cam.position) - feet;
    let dist = to_cam.length() as f32;
    if dist >= 32.0 {
        if real {
            CLIENT.lock().unwrap().trails.remove(name);
        }
        return;
    }
    // The camera's axes in the body's frame (as `draw_plate` turns the name tag).
    let turn = Mat3::from_rotation_y((mob.body_yaw + 180.0 - cam.yaw).to_radians()) * Mat3::from_rotation_x((-cam.pitch).to_radians());
    let (right, up) = (turn * Vec3::X, turn * Vec3::Y);
    let (cat, low, light) = (catalog(), crate::fx::low(), draw.light);
    let fit = body_fit(height, radius);
    if let Some(item) = w.aura.as_deref().and_then(|id| cat.item(id)) {
        if let Some(p) = item.particle.as_ref() {
            // A legendary aura: its own look, or a plain one in its tint.
            let look = legend(&item.id).or_else(|| (item.rarity == Rarity::Legendary).then_some(Legend { theme: 0, accent: p.tint_rgb(), mote: "capipimon:entity/fx_spark" }));
            if p.glow || look.is_some() {
                draw.light = 0xF0F0;
            }
            let t = mob.age / 20.0 * p.speed;
            // Legendary auras carry half as many again of their own sprite.
            let count = if look.is_some() { p.count + p.count / 2 } else { p.count };
            for i in 0..budget_for(count, dist, low, fit.k) {
                let (at, grow) = aura_at(p.motion, i, t, fit.height.max(0.4), fit.ring);
                if grow > 0.05 {
                    billboard(host, draw, &p.sprite, at, right, up, fit.size(p.size) * grow, p.tint_rgb());
                }
            }
            if let Some(look) = look {
                // Motes of the accent colour streaming up out of the ground ring, quicker and
                // smaller than the main sprites; drawn on Low FX too, where the pass is off.
                let tm = mob.age / 20.0 * 1.4;
                for i in 0..budget_for(10, dist, low, fit.k) {
                    let (at, grow) = aura_at(Motion::Rise, i + 40, tm, fit.height * 1.25, fit.ring * 1.15);
                    if grow > 0.05 {
                        billboard(host, draw, look.mote, at, right, up, fit.size(0.075) * grow, look.accent);
                    }
                }
                if real {
                    note_legend(name, LegendBody { feet, fit, look, tint: p.tint_rgb(), seed: name_seed(name), dist, at: Instant::now() });
                }
            }
            draw.light = light;
        }
    }
    if !real {
        return;
    }
    let Some(p) = w.trail.as_deref().and_then(|id| cat.item(id)).and_then(|i| i.particle.as_ref()) else { return };
    let mut c = CLIENT.lock().unwrap();
    // Standing in the wardrobe, where nothing walks: a trail streams out behind to show what it looks like.
    if local && c.view.is_some() && mob.walk_speed <= 0.02 {
        drop(c);
        if p.glow {
            draw.light = 0xF0F0;
        }
        let n = budget_for(p.count, dist, low, fit.k).max(1);
        let t = mob.age / 20.0;
        let (wide, spacing) = trail_spread(&fit, p.spacing);
        for k in 0..n {
            let age = (t / trail_life(&fit, p.life) + k as f32 / n as f32).fract();
            let s = (k as f32 * 0.618_034).fract();
            let drift = match p.motion {
                Motion::Fall => -0.25 * age,
                _ => 0.35 * age,
            } * fit.k.sqrt();
            // The body's frame: +z is forward, so the puffs fall back along -z.
            let at = Vec3::new(((s * 37.0).sin()) * wide, drift + trail_height(&fit, s), -(fit.ring * 0.6 + age * n as f32 * spacing));
            billboard(host, draw, &p.sprite, at, right, up, fit.size(p.size) * (1.0 - age * 0.8), p.tint_rgb());
        }
        draw.light = light;
        return;
    }
    let trail = c.trails.entry(name.to_string()).or_default();
    let now = Instant::now();
    let (wide, spacing) = trail_spread(&fit, p.spacing);
    let moved = trail.last.is_none_or(|l| l.distance(feet) >= f64::from(spacing));
    if moved && mob.walk_speed > 0.02 {
        trail.count = trail.count.wrapping_add(1);
        trail.points.push_back(Point { at: feet, born: now, seed: trail.count });
        trail.last = Some(feet);
    }
    let cap = budget_for(p.count, dist, low, fit.k) as usize;
    while trail.points.len() > cap {
        trail.points.pop_front();
    }
    let life = trail_life(&fit, p.life);
    trail.points.retain(|q| now.duration_since(q.born).as_secs_f32() < life);
    if p.glow {
        draw.light = 0xF0F0;
    }
    // World to the body's frame: the pose stack is T(feet) * rotY(-body yaw).
    let back = Mat3::from_rotation_y(mob.body_yaw.to_radians());
    for q in &trail.points {
        let age = now.duration_since(q.born).as_secs_f32() / life;
        let s = (q.seed as f32 * 0.618_034).fract();
        let drift = match p.motion {
            Motion::Fall => -0.25 * age,
            _ => 0.35 * age,
        } * fit.k.sqrt();
        let spread = Vec3::new(((s * 37.0).sin()) * wide, drift + trail_height(&fit, s), ((s * 91.0).cos()) * wide);
        billboard(host, draw, &p.sprite, back * (q.at - feet).as_vec3() + spread, right, up, fit.size(p.size) * (1.0 - age * 0.8), p.tint_rgb());
    }
    draw.light = light;
}

/// A trail prize's preview during a capsule opening (Onii, 2026-11-20): three heads of the trail's
/// own particles circle where the capsule burst, each with a tail of fading puffs behind it, like
/// the trail behind a walking Pokémon. Drawn with the local body (`lib.rs`), in its frame.
pub fn capsule_trail(host: &dyn Host, draw: &mut BlockEntityDraw, mob: &horizon_client::mod_api::NativeMobRender<'_>) {
    let Some((id, at, t)) = crate::fx::capsule_trail() else { return };
    let Some(cam) = host.camera() else { return };
    let Some(feet) = crate::play::local_feet().map(DVec3::from_array) else { return };
    let cat = catalog();
    let Some(p) = cat.item(&id).and_then(|i| i.particle.as_ref()) else { return };
    let turn = Mat3::from_rotation_y((mob.body_yaw + 180.0 - cam.yaw).to_radians()) * Mat3::from_rotation_x((-cam.pitch).to_radians());
    let (right, up) = (turn * Vec3::X, turn * Vec3::Y);
    let back = Mat3::from_rotation_y(mob.body_yaw.to_radians());
    let light = draw.light;
    draw.light = 0xF0F0;
    // Grows in over the first half second after the burst.
    let grow = (t / 0.5).clamp(0.0, 1.0);
    const HEADS: usize = 3;
    const TAIL: usize = 9;
    for h in 0..HEADS {
        let phase = h as f32 * std::f32::consts::TAU / HEADS as f32;
        for k in 0..TAIL {
            let lag = k as f32 * 0.07;
            let a = t * 2.4 + phase - lag * 2.4;
            let r = 0.42 * grow;
            let bob = (a * 1.7 + phase).sin() * 0.08;
            let drift = match p.motion {
                Motion::Fall => -0.12 * lag,
                _ => 0.12 * lag,
            };
            let world = at + DVec3::new(f64::from(a.cos() * r), f64::from(bob + drift), f64::from(a.sin() * r));
            let size = 0.07 * (1.0 - k as f32 / TAIL as f32 * 0.8) * grow * (p.size / 0.1).clamp(0.6, 1.6);
            billboard(host, draw, &p.sprite, back * (world - feet).as_vec3(), right, up, size, p.tint_rgb());
        }
    }
    draw.light = light;
}

/// How far a trail's puffs scatter sideways, and the blocks walked between them, for a body: as
/// wide as the body (a quarter block on a Pikachu), and further apart behind a big stride.
pub fn trail_spread(fit: &Fit, spacing: f32) -> (f32, f32) {
    ((0.25 * fit.k).min(fit.ring * 0.9).max(0.12), spacing * fit.k.sqrt().clamp(0.8, 2.0))
}

/// How long a trail's puffs last for a body: longer behind a big one, so its trail stretches as far
/// back for its size (up to half as long again).
pub fn trail_life(fit: &Fit, life: f32) -> f32 {
    life.max(0.1) * fit.k.sqrt().clamp(1.0, 1.5)
}

/// How high puff `s` (0-1) of a trail floats over the ground: low on the body, up to its middle.
pub fn trail_height(fit: &Fit, s: f32) -> f32 {
    fit.height.min(1.2 * fit.k) * (0.1 + 0.4 * s)
}

/// An Alpha's aura (`npcalpha`): red embers rising around it and sparks circling at mid height,
/// full-bright so it reads in a dark dungeon. At the draw origin (the feet, in the body's frame,
/// as `particles`); `height` and `radius` are the drawn body's, already scaled by its size.
pub fn alpha_aura(host: &dyn Host, draw: &mut BlockEntityDraw, mob: &horizon_client::mod_api::NativeMobRender<'_>, height: f32, radius: f32) {
    let Some(cam) = host.camera() else { return };
    let turn = Mat3::from_rotation_y((mob.body_yaw + 180.0 - cam.yaw).to_radians()) * Mat3::from_rotation_x((-cam.pitch).to_radians());
    let (right, up) = (turn * Vec3::X, turn * Vec3::Y);
    let light = draw.light;
    draw.light = 0xF0F0;
    let t = mob.age / 20.0;
    let low = crate::fx::low();
    let (embers, sparks) = if low { (8, 3) } else { (18, 6) };
    let h = height.max(0.6);
    for i in 0..embers {
        let (at, grow) = aura_at(Motion::Rise, i, t * 1.3, h, radius);
        if grow > 0.05 {
            // Deep red to orange as they rise.
            let tint = if i % 3 == 0 { 0xFF_7A1F } else { 0xFF_2A1A };
            billboard(host, draw, "capipimon:entity/fx_ember", at, right, up, 0.16 * grow * (0.8 + h * 0.08), tint);
        }
    }
    for i in 0..sparks {
        let (at, _) = aura_at(Motion::Orbit, i, t, h, radius);
        billboard(host, draw, "capipimon:entity/fx_spark", at, right, up, 0.14, 0xFF_4040);
    }
    draw.light = light;
}

/// `/cosmetic others on|off` (client only) and `/cosmetic seen` (what this client knows, for tests).
pub fn client_command(host: &mut dyn Host, command: &str) -> bool {
    let words: Vec<&str> = command.trim_start_matches('/').split_whitespace().collect();
    if words.first() != Some(&"cosmetic") {
        return false;
    }
    match words.get(1).copied() {
        Some("others") => {
            let hide = match words.get(2).copied() {
                Some("off") => true,
                Some("on") => false,
                _ => !CLIENT.lock().unwrap().hide_others,
            };
            set_hidden(host, hide);
            true
        }
        Some("seen") => {
            let c = CLIENT.lock().unwrap();
            let mut seen: Vec<String> = c.worn.iter().filter(|(_, w)| !w.empty()).map(|(n, w)| format!("{n}={}", w.hat.as_deref().or(w.aura.as_deref()).or(w.trail.as_deref()).unwrap_or("-"))).collect();
            seen.sort();
            let line = format!("Cosmetics seen: {}", if seen.is_empty() { "none".to_string() } else { seen.join(" ") });
            drop(c);
            host.client_action_bar(Text::literal(line), false);
            true
        }
        // For tests: the size this client's own cosmetics are fitted to.
        Some("body") => {
            let line = match local_body() {
                Some(b) => {
                    let f = body_fit(b.height, b.radius);
                    format!("Body: height {:.2} reach {:.2} ring {:.2} k {:.2}", b.height, b.radius, f.ring, f.k)
                }
                None => "Body: not drawn".to_string(),
            };
            host.client_action_bar(Text::literal(line), false);
            true
        }
        _ => false,
    }
}

fn set_hidden(host: &mut dyn Host, hide: bool) {
    CLIENT.lock().unwrap().hide_others = hide;
    host.set_setting(crate::MOD_ID, SETTING, if hide { "hide" } else { "show" });
    host.client_action_bar(Text::literal(if hide { "Other players' cosmetics: hidden" } else { "Other players' cosmetics: shown" }), false);
}

/// Reads the hide-others setting the first time the HUD draws.
fn read_setting(host: &dyn Host) {
    let mut c = CLIENT.lock().unwrap();
    if !c.read {
        c.read = true;
        c.hide_others = host.setting(crate::MOD_ID, SETTING).as_deref() == Some("hide");
    }
}

// ---------------------------------------------------------------- the wardrobe

/// Keys while the wardrobe is open (all of them, so moves don't fire).
pub fn key(host: &mut dyn Host, key: &str) -> bool {
    let cat = catalog();
    let mut c = CLIENT.lock().unwrap();
    let hide = c.hide_others;
    let Some(v) = c.view.as_mut() else { return false };
    let mut send: Option<String> = None;
    let list = rows(&cat, v.tab, &v.owned, v.filter);
    let n = list.len().max(1);
    let nf = filters(&cat, v.tab).len();
    let mut note: Option<String> = None;
    match key {
        "key.keyboard.up" | "key.keyboard.w" => v.sel = (v.sel + n - 1) % n,
        "key.keyboard.down" | "key.keyboard.s" => v.sel = (v.sel + 1) % n,
        "key.keyboard.page.up" | "key.keyboard.z" => v.sel = page(v.sel, n, false),
        "key.keyboard.page.down" | "key.keyboard.c" => v.sel = page(v.sel, n, true),
        "key.keyboard.left" | "key.keyboard.a" => {
            v.tab = (v.tab + TABS.len() - 1) % TABS.len();
            v.sel = 0;
            v.filter = 0;
        }
        "key.keyboard.right" | "key.keyboard.d" => {
            v.tab = (v.tab + 1) % TABS.len();
            v.sel = 0;
            v.filter = 0;
        }
        "key.keyboard.q" => {
            v.filter = (v.filter + nf - 1) % nf;
            v.sel = 0;
        }
        "key.keyboard.r" => {
            v.filter = (v.filter + 1) % nf;
            v.sel = 0;
        }
        "key.keyboard.enter" | "key.keyboard.space" | "key.keyboard.keypad.enter" => {
            if let Some(item) = list.get(v.sel) {
                if v.tab == 3 {
                    send = Some(format!("ward\tbuy\t{}", item.id));
                } else if !v.owned.contains(&item.id) {
                    note = Some(format!("Earn it: {}.", how_to_get(item)));
                } else if v.worn.get(item.slot) == Some(item.id.as_str()) {
                    send = Some(format!("ward\tunequip\t{}", item.id));
                } else {
                    send = Some(format!("ward\tequip\t{}", item.id));
                }
            }
        }
        "key.keyboard.h" => {
            drop(c);
            set_hidden(host, !hide);
            return true;
        }
        "key.keyboard.backspace" | "key.keyboard.x" => {
            c.view = None;
            drop(c);
            let _ = host.net_send_to_server(CHANNEL, b"ward\tclose");
            return true;
        }
        _ => {}
    }
    if let Some(t) = note {
        c.note = Some((t, Instant::now()));
    }
    drop(c);
    if let Some(s) = send {
        let _ = host.net_send_to_server(CHANNEL, s.as_bytes());
    }
    true
}

fn how_to_get(item: &Item) -> String {
    let parts: Vec<String> = item
        .sources
        .iter()
        .map(|s| match s.split_once(':') {
            None if s == "capsule" && item.slot == Slot::Hat => "the shard shop".to_string(),
            None if s == "capsule" => "a dungeon capsule or the shop".to_string(),
            None if s == "prestige" => "reach Prestige 2".to_string(),
            Some(("raid", "gyarados")) => "beat Sakana's Gyarados".to_string(),
            Some(("raid", "rabbit")) => "beat the Rabbit Raid".to_string(),
            Some(("raid", _)) => "a raid".to_string(),
            Some(("cameo", _)) => "beat a talent cameo".to_string(),
            Some(("seal", _)) => "clear this gym's dungeon".to_string(),
            Some(("season", step)) => format!("the season track ({})", step.replace('_', " ")),
            _ => s.clone(),
        })
        .collect();
    parts.join(" or ")
}

const FRAME: u32 = 0xF02A_2348;
const HEAD: u32 = 0xFF6A_4FB8;
const ROW: u32 = 0xE6F4_F0FC;
const SEL: u32 = 0xFFFF_D23F;

pub fn hud(host: &mut dyn Host, b: &mut HudBatch) {
    read_setting(&*host);
    let c = CLIENT.lock().unwrap();
    let Some(v) = c.view.as_ref() else { return };
    let cat = catalog();
    let list = rows(&cat, v.tab, &v.owned, v.filter);
    // On the left: the Pokémon stays visible in the middle of the screen, trying things on.
    let (x, w, h) = (8.0f32, 216.0f32, 220.0f32);
    let y = ((b.height - h) / 2.0).max(4.0);
    pill(host, b, x, y, w, h, FRAME);
    pill(host, b, x + 3.0, y + 3.0, w - 6.0, 14.0, HEAD);
    host.text(b, "Wardrobe", [x + 8.0, y + 6.0], 0xFFFFFF, true);
    let bal = format!("\u{25C6} {} shards", v.shards);
    let bw = host.text_width(&bal);
    host.text(b, &bal, [x + w - 8.0 - bw, y + 6.0], 0x9CE8FF, true);
    let tw = (w - 10.0) / TABS.len() as f32;
    for (i, name) in TABS.iter().enumerate() {
        let tx = x + 5.0 + i as f32 * tw;
        let on = i == v.tab;
        rect(host, b, tx, y + 20.0, tw - 2.0, 11.0, if on { SEL } else { 0xFF3C_3468 });
        let lw = host.text_width(name);
        host.text(b, name, [tx + (tw - 2.0 - lw) / 2.0, y + 22.0], if on { 0x202830 } else { 0xE0E0F0 }, !on);
    }
    // The filter, how much of it you own, and where the cursor is.
    let fs = filters(&cat, v.tab);
    let f = fs.get(v.filter).copied().unwrap_or(Filter::All);
    let have = list.iter().filter(|i| v.owned.contains(&i.id)).count();
    let what = if fs.len() > 1 { format!("\u{25C0} {} \u{25B6}", filter_name(&cat, f)) } else { filter_name(&cat, f).to_string() };
    let what = if v.tab == 3 { what } else { format!("{what}  {have}/{}", list.len()) };
    host.text(b, &fit(host, &what, w - 60.0), [x + 7.0, y + 34.0], 0xFFE9A0, true);
    let at = if list.is_empty() { String::new() } else { format!("{}/{}", v.sel + 1, list.len()) };
    let aw = host.text_width(&at);
    host.text(b, &at, [x + w - 8.0 - aw, y + 34.0], 0xC8D0E0, true);
    let top = y + 46.0;
    let start = v.sel.saturating_sub(SHOWN / 2).min(list.len().saturating_sub(SHOWN));
    for (i, item) in list.iter().enumerate().skip(start).take(SHOWN) {
        let ry = top + (i - start) as f32 * 11.0;
        let owned = v.owned.contains(&item.id);
        rect(host, b, x + 5.0, ry - 1.0, w - 10.0, 10.0, if i == v.sel { SEL } else { ROW });
        rect(host, b, x + 8.0, ry + 1.0, 4.0, 6.0, 0xFF00_0000 | rarity_color(item.rarity));
        let dim = if owned || i == v.sel { 0x202830 } else { 0x7A8494 };
        host.text(b, &fit(host, &item.name, 120.0), [x + 15.0, ry], dim, false);
        let tag = if v.tab == 3 {
            format!("{} \u{25C6}", SHOP_COST[rank(item.rarity)])
        } else if v.worn.get(item.slot) == Some(item.id.as_str()) {
            "Worn".to_string()
        } else if owned {
            "Owned".to_string()
        } else {
            "Locked".to_string()
        };
        let sw = host.text_width(&tag);
        host.text(b, &tag, [x + w - 9.0 - sw, ry], if v.worn.get(item.slot) == Some(item.id.as_str()) { 0x1E7A38 } else { 0x404A58 }, false);
    }
    if list.is_empty() {
        let empty = match (v.tab, f) {
            (3, Filter::All) => "You own everything in the shop.",
            (_, Filter::Owned) => "None owned yet.",
            _ => "Nothing here yet.",
        };
        host.text(b, empty, [x + 10.0, top + 5.0], 0xE0E0F0, true);
    }
    let info = top + SHOWN as f32 * 11.0 + 3.0;
    if let Some(item) = list.get(v.sel) {
        let rarity = match cat.set_of(&item.id) {
            Some(set) => format!("{:?} \u{00B7} {set}", item.rarity),
            None => format!("{:?}", item.rarity),
        };
        host.text(b, &rarity, [x + 7.0, info], rarity_color(item.rarity), true);
        host.text(b, &fit(host, &how_to_get(item), w - 14.0), [x + 7.0, info + 11.0], 0xC8D0E0, true);
    }
    let others = if c.hide_others { "H show others" } else { "H hide others" };
    let lines = ["A/D tab  W/S pick  Z/C page  Q/R filter".to_string(), "Mouse turn  Wheel zoom".to_string(), format!("Space use  {others}  X back")];
    for (n, line) in lines.iter().enumerate() {
        let line = fit(host, line, w - 10.0);
        let lw = host.text_width(&line);
        host.text(b, &line, [x + (w - lw) / 2.0, y + h - 43.0 + n as f32 * 10.0], 0xD8E4F4, true);
    }
    if let Some((t, at)) = &c.note {
        if at.elapsed().as_secs_f32() < 4.0 {
            let t = fit(host, t, w - 10.0);
            let nw = host.text_width(&t);
            host.text(b, &t, [x + (w - nw) / 2.0, y + h - 12.0], 0xFFE9A0, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests below share the client's state (`hide_others`), so they take turns.
    static SHARED: Mutex<()> = Mutex::new(());

    #[test]
    fn the_budget_falls_with_distance_and_low_fx() {
        assert_eq!(budget(10, 5.0, false), 10);
        assert_eq!(budget(10, 20.0, false), 5);
        assert_eq!(budget(10, 40.0, false), 0);
        assert_eq!(budget(10, 5.0, true), 5);
        assert_eq!(budget(99, 1.0, false), 16, "hard cap");
    }

    #[test]
    fn cosmetics_grow_with_the_body() {
        let pichu = body_fit(0.56, 0.22);
        let pikachu = body_fit(0.9, 0.4);
        let moltres = body_fit(2.6, 3.5);
        let wailord = body_fit(8.0, 6.0);
        assert!(pichu.k < 0.85 && pichu.k >= 0.6, "small shrinks, but stays readable: {pichu:?}");
        assert!((0.85..1.2).contains(&pikachu.k), "medium is the catalogue's own size: {pikachu:?}");
        assert!(moltres.k > 2.5 && wailord.k >= moltres.k && wailord.k <= 4.5, "{moltres:?} {wailord:?}");
        assert!(moltres.ring <= 2.6 * 1.1 + 0.5 + 1e-4, "a wingspan doesn't push the ring out: {moltres:?}");
        assert!(moltres.size(0.2) > 0.35 && pichu.size(0.2) < 0.2);
        assert!(budget_for(8, 5.0, false, moltres.k) > budget(8, 5.0, false));
        assert!(budget_for(99, 1.0, false, 9.0) <= 32, "hard cap");
        let (w_small, s_small) = trail_spread(&pikachu, 0.45);
        let (w_big, s_big) = trail_spread(&moltres, 0.45);
        assert!(w_big > w_small * 2.0 && s_big > s_small, "a big trail is wider and spaced out");
        assert!(trail_height(&moltres, 1.0) > trail_height(&pikachu, 1.0) * 2.0);
    }

    #[test]
    fn every_legendary_aura_has_a_look_and_flares_stay_in_their_period() {
        for id in ["aura.legend_fire", "aura.legend_water", "aura.storm_legend", "aura.tide_legend", "aura.legend_ice", "aura.legend_psychic", "aura.legend_ground",
                   "aura.legend_dragon", "aura.legend_rock", "aura.legend_steel", "aura.legend_grass", "aura.legend_fairy", "aura.legend_fighting", "aura.legend_ghost",
                   "aura.legend_dark", "aura.legend_poison", "aura.legend_normal", "aura.legend_bug", "aura.plug_sparks", "aura.stars"] {
            let l = legend(id).unwrap_or_else(|| panic!("{id} has no look"));
            assert!(l.theme <= STARLIGHT);
        }
        assert!(legend("aura.petals").is_none());
        for seed in [0, 7, 99_999, u32::MAX] {
            for secs in [0.0, 3.3, 999.9] {
                let (t, p) = flare(seed, secs);
                assert!((4.5..=6.5).contains(&p) && (0.0..p).contains(&t), "{seed} {secs}: {t} of {p}");
            }
        }
    }

    #[test]
    fn aura_particles_stay_around_the_body() {
        for m in [Motion::Rise, Motion::Fall, Motion::Orbit, Motion::Flicker] {
            for i in 0..16 {
                for t in [0.0, 1.3, 17.7] {
                    let (p, g) = aura_at(m, i, t, 2.0, 1.0);
                    assert!(p.is_finite() && (0.0..=1.01).contains(&g), "{m:?} {p} {g}");
                    assert!(p.y > -0.3 && p.y < 2.8, "{m:?} y {}", p.y);
                    assert!(Vec3::new(p.x, 0.0, p.z).length() < 1.6, "{m:?} {p}");
                }
            }
        }
    }

    fn shelf() -> Catalog {
        let mut c = Catalog::parse(r#"{"schema":1,"items":[
          {"id":"hat.a","name":"A","slot":"hat","rarity":"common","sources":["capsule"],"hat":{"geo":"a","sprite":"s"}},
          {"id":"hat.b","name":"B","slot":"hat","rarity":"legendary","sources":["capsule"],"hat":{"geo":"b","sprite":"s"}},
          {"id":"hat.c","name":"C","slot":"hat","rarity":"rare","sources":["capsule"],"hat":{"geo":"c","sprite":"s"}},
          {"id":"hat.d","name":"D","slot":"hat","rarity":"common","sources":["raid:rabbit"],"hat":{"geo":"d","sprite":"s"}},
          {"id":"trail.t","name":"T","slot":"trail","rarity":"common","sources":["capsule"],"particle":{"sprite":"s","count":3,"size":0.1}}
        ]}"#)
        .unwrap();
        c.sets = parse_sets(r#"{"schema":1,"sets":[{"name":"Unit","items":["hat.b","hat.a"]},{"name":"Other","items":["hat.c","hat.d"]}]}"#, &c).unwrap();
        c
    }

    fn ids(list: &[&Item]) -> Vec<String> {
        list.iter().map(|i| i.id.clone()).collect()
    }

    #[test]
    fn filters_step_through_owned_and_the_sets() {
        let c = shelf();
        assert_eq!(filters(&c, 0), [Filter::All, Filter::Owned, Filter::Set(0), Filter::Set(1)]);
        assert_eq!(filters(&c, 2), [Filter::All, Filter::Owned], "no sets on the trail tab");
        assert_eq!(filters(&c, 3), [Filter::All, Filter::Set(0), Filter::Set(1)], "nothing owned is for sale");
        let owned: HashSet<String> = ["hat.a".to_string()].into();
        assert_eq!(ids(&rows(&c, 0, &owned, 0)), ["hat.a", "hat.d", "hat.c", "hat.b"], "owned first, then by rarity");
        assert_eq!(ids(&rows(&c, 0, &owned, 1)), ["hat.a"]);
        assert_eq!(ids(&rows(&c, 0, &owned, 2)), ["hat.a", "hat.b"]);
        assert_eq!(ids(&rows(&c, 3, &owned, 1)), ["hat.b"], "the shop under a set: unowned, buyable");
        assert_eq!(ids(&rows(&c, 3, &owned, 2)), ["hat.c"], "the raid hat isn't for sale");
        assert_eq!(ids(&rows(&c, 0, &owned, 99)), ids(&rows(&c, 0, &owned, 0)), "a stale filter falls back to All");
        assert_eq!(filter_name(&c, Filter::Set(1)), "Other");
    }

    #[test]
    fn page_jumps_stop_at_the_ends() {
        assert_eq!(page(0, 116, true), SHOWN);
        assert_eq!(page(110, 116, true), 115);
        assert_eq!(page(4, 116, false), 0);
        assert_eq!(page(0, 0, true), 0);
    }

    #[test]
    fn hidden_others_show_nothing_but_your_own_stays() {
        let _turn = SHARED.lock().unwrap_or_else(|e| e.into_inner());
        let mut c = CLIENT.lock().unwrap();
        c.worn.insert("Other".into(), Worn { hat: Some("hat.a".into()), ..Default::default() });
        c.worn.insert("Me".into(), Worn { hat: Some("hat.a".into()), ..Default::default() });
        c.hide_others = true;
        drop(c);
        assert_eq!(worn_by("Other", false), None);
        assert!(worn_by("Me", true).is_some());
        let mut c = CLIENT.lock().unwrap();
        c.hide_others = false;
        drop(c);
        assert!(worn_by("Other", false).is_some());
    }

    #[test]
    fn loadouts_arrive_from_the_server() {
        let _turn = SHARED.lock().unwrap_or_else(|e| e.into_inner());
        assert!(heard(&["cosm", "Sam", "hat.a", "-", "trail.t"]));
        assert_eq!(worn_by("Sam", false).unwrap(), Worn { hat: Some("hat.a".into()), aura: None, trail: Some("trail.t".into()) });
        assert!(heard(&["cosm", "Sam", "-", "-", "-"]));
        assert_eq!(worn_by("Sam", false), None);
    }
}
