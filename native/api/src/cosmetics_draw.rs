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
    let mut c = CLIENT.lock().unwrap();
    let part = |s: &str| (s != "-").then(|| s.to_string());
    match f {
        ["cosm", name, hat, aura, trail, ..] => {
            let worn = Worn { hat: part(hat), aura: part(aura), trail: part(trail) };
            if worn.trail.is_none() {
                c.trails.remove(*name);
            }
            c.worn.insert(name.to_string(), worn);
        }
        ["ward", "-", ..] => c.view = None,
        ["ward", shards, hat, aura, trail, owned, ..] => {
            let (tab, sel, filter) = c.view.as_ref().map_or((0, 0, 0), |v| (v.tab, v.sel, v.filter));
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
    true
}

pub fn forget() {
    let mut c = CLIENT.lock().unwrap();
    let (hide, read) = (c.hide_others, c.read);
    *c = CosClient { hide_others: hide, read, ..Default::default() };
}

pub fn is_open() -> bool {
    CLIENT.lock().unwrap().view.is_some()
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
    let share = if dist < 16.0 {
        1.0
    } else if dist < 32.0 {
        0.5
    } else {
        0.0
    };
    let share = if low { share * 0.5 } else { share };
    ((count.min(16) as f32 * share).ceil()) as u32
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
pub fn particles(host: &dyn Host, draw: &mut BlockEntityDraw, w: &Worn, name: &str, height: f32, radius: f32, mob: &horizon_client::mod_api::NativeMobRender<'_>, real: bool) {
    if w.aura.is_none() && (w.trail.is_none() || !real) {
        return;
    }
    let Some(cam) = host.camera() else { return };
    let Some(e) = host.client_entities().into_iter().find(|e| e.player_name.as_deref() == Some(name)) else { return };
    let lerp = |i: usize| e.previous_position[i] + (e.position[i] - e.previous_position[i]) * f64::from(mob.partial);
    let feet = DVec3::new(lerp(0), lerp(1), lerp(2));
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
    if let Some(p) = w.aura.as_deref().and_then(|id| cat.item(id)).and_then(|i| i.particle.as_ref()) {
        if p.glow {
            draw.light = 0xF0F0;
        }
        let t = mob.age / 20.0 * p.speed;
        for i in 0..budget(p.count, dist, low) {
            let (at, grow) = aura_at(p.motion, i, t, height.max(0.4), radius.max(0.3));
            if grow > 0.05 {
                billboard(host, draw, &p.sprite, at, right, up, p.size * grow, p.tint_rgb());
            }
        }
        draw.light = light;
    }
    if !real {
        return;
    }
    let Some(p) = w.trail.as_deref().and_then(|id| cat.item(id)).and_then(|i| i.particle.as_ref()) else { return };
    let mut c = CLIENT.lock().unwrap();
    let trail = c.trails.entry(name.to_string()).or_default();
    let now = Instant::now();
    let moved = trail.last.is_none_or(|l| l.distance(feet) >= f64::from(p.spacing));
    if moved && mob.walk_speed > 0.02 {
        trail.count = trail.count.wrapping_add(1);
        trail.points.push_back(Point { at: feet, born: now, seed: trail.count });
        trail.last = Some(feet);
    }
    let cap = budget(p.count, dist, low) as usize;
    while trail.points.len() > cap {
        trail.points.pop_front();
    }
    trail.points.retain(|q| now.duration_since(q.born).as_secs_f32() < p.life);
    if p.glow {
        draw.light = 0xF0F0;
    }
    // World to the body's frame: the pose stack is T(feet) * rotY(-body yaw).
    let back = Mat3::from_rotation_y(mob.body_yaw.to_radians());
    for q in &trail.points {
        let age = now.duration_since(q.born).as_secs_f32() / p.life;
        let s = (q.seed as f32 * 0.618_034).fract();
        let drift = match p.motion {
            Motion::Fall => -0.25 * age,
            _ => 0.35 * age,
        };
        let spread = Vec3::new(((s * 37.0).sin()) * 0.25, drift + height.min(1.2) * (0.1 + 0.4 * s), ((s * 91.0).cos()) * 0.25);
        billboard(host, draw, &p.sprite, back * (q.at - feet).as_vec3() + spread, right, up, p.size * (1.0 - age * 0.8), p.tint_rgb());
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
        "key.keyboard.enter" | "key.keyboard.f" | "key.keyboard.space" | "key.keyboard.keypad.enter" => {
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
        "key.keyboard.escape" | "key.keyboard.backspace" | "key.keyboard.e" => {
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

/// Standing still while it's open (WASD move the cursor).
pub fn movement_input(event: &mut MovementInputEvent) {
    if is_open() {
        event.forward = 0.0;
        event.strafe = 0.0;
    }
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
    let (x, w, h) = (8.0f32, 216.0f32, 210.0f32);
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
    let lines = ["A/D tab  W/S pick  Z/C page  Q/R filter".to_string(), format!("Enter use  {others}  Esc close")];
    for (n, line) in lines.iter().enumerate() {
        let line = fit(host, line, w - 10.0);
        let lw = host.text_width(&line);
        host.text(b, &line, [x + (w - lw) / 2.0, y + h - 33.0 + n as f32 * 10.0], 0xD8E4F4, true);
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
