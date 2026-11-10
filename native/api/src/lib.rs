//! Capipimon models: draws creatures' Bedrock (Blockbench) models on Horizon mobs.
//!
//! The models are third-party art under their own licences, so none are compiled in: each species
//! in the model library's `index.json` is read when it's first drawn (`cobblemon`), and the texture
//! sprite comes from the content mod's `assets/capipimon/textures/entity/`.

pub mod bedrock;
pub mod emotes;
pub mod fx;
#[cfg(feature = "capipimon_server")]
#[path = "server/lab.rs"]
pub mod lab;
pub mod molang;
pub mod pack;
pub mod play;
pub mod standin;

use play::cosmetics;
use bedrock::{Animation, Model};
use horizon_client::mod_api::{
    AttackEntityEvent, CameraShakeEvent, ClientDisconnectingEvent, GuiLayerPreEvent, CommandEvent, FrameEvent, InjectCapipimonModelsDropEvent,
    InjectCapipimonModelsSwapHandsEvent, MovementInputEvent, PlayerItemHeldEvent, PlayerPickItemEvent, RenderHooks, VisualsEvent, WheelEvent,
    CameraDistanceEvent, CameraPoseEvent, Flow, Hooks, Host, HudEvent, KeyEvent, LivingIncomingDamageEvent,
    MobSubmitEvent, InteractionKeyEvent, NetFromClientEvent, NetFromServerEvent, PerformCommandEvent, PlayerDiedEvent,
    PlayerLoggedInEvent, PlayerLoggedOutEvent, PlayerRenderListenedEvent, PlayerRespawnedEvent, PlayerSubmitEvent,
    PlayerChatEvent, PlayerSwapHandsEvent, RegisterCommandsEvent, RenderHandEvent, RenderNameTagEvent, RightClickBlockEvent, ServerTickEvent,
    LeftClickBlockEvent, MobLayerEvent, Text, ToastAddEvent, UseItemEvent, UseItemOnEvent,
};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};

/// This mod's id, for its settings.
pub const MOD_ID: &str = "capipimon_models";

pub fn hooks() -> Box<dyn Hooks> {
    Box::new(Mod)
}

struct Mod;

struct Creature {
    model: Model,
    idle: Option<Animation>,
    walk: Option<Animation>,
    /// Everything else (cast animations such as `physical` and `special`).
    extra: HashMap<String, Animation>,
    /// A made-up walk cycle (`Model::gait`) when there's no walk animation.
    gait: Vec<(usize, glam::Vec3)>,
    /// The bones a made-up attack pose moves, for casts the model has no animation for.
    rig: standin::Rig,
    /// How far the model reaches out from its middle at rest, unscaled blocks: auras ring this.
    radius: f32,
}

fn build(geo: &str, anim: &str, idle: &str, walk: &str) -> Result<Creature, String> {
    let model = Model::parse(geo)?;
    let mut anims = Animation::parse_all(anim)?;
    // The named animation, else the nearest one the file has (a walk-less model may still run,
    // fly or swim; an idle-less one may still have a battle stance), else nothing.
    let mut first = |names: &[&str]| names.iter().find_map(|n| anims.remove(*n));
    let idle = first(&[idle, "ground_idle", "battle_idle", "air_idle", "water_idle"]);
    let walk = first(&[walk, "ground_walk", "ground_run", "air_fly", "water_swim"]);
    let gait = if walk.is_none() { model.gait() } else { Vec::new() };
    let rig = standin::Rig::of(&model);
    let mut radius = 0.0f32;
    model.emit(&model.pose(None, 0.0), 1.0, |corners, _| {
        for p in corners {
            radius = radius.max(p[0].abs()).max(p[2].abs());
        }
    });
    Ok(Creature { model, idle, walk, extra: anims, gait, rig, radius })
}

// ---------------------------------------------------------------- the model library
//
// Cobblemon's species and the add-on packs' are in `cobblemon-models/` in the content mod, and
// they're read from the content pack or the instance's copy the first time a species is drawn.

/// The content mod's folder in this game's instance. A registry install has none, and `pack::read`
/// finds its files in the content pack.
pub(crate) fn content_folder(host: &dyn Host) -> std::path::PathBuf {
    host.instance_dir().join("horizon-mods").join("capipimon")
}

struct Entry {
    scale: f32,
    idle: String,
    walk: String,
}

/// `index.json`: species → scale and idle / walk animation names. Empty if the file isn't there.
fn cobblemon_index(host: &dyn Host) -> &'static HashMap<String, Entry> {
    static INDEX: OnceLock<HashMap<String, Entry>> = OnceLock::new();
    INDEX.get_or_init(|| {
        let text = pack::read(&content_folder(host), "cobblemon-models/index.json").unwrap_or_default();
        let list: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap_or_default();
        list.iter()
            .filter_map(|e| {
                let s = |k: &str| e[k].as_str().unwrap_or("").to_string();
                Some((s("id"), Entry { scale: e["scale"].as_f64()? as f32, idle: s("idle"), walk: s("walk") }))
            })
            .collect()
    })
}

/// A loaded Cobblemon species, kept for the rest of the game.
pub struct Cobbled {
    sprite: String,
    scale: f32,
    creature: Creature,
}

/// The Cobblemon model of `species`: None while it loads (on its own thread the first time it is
/// asked for, so reading and parsing its files never holds up a frame) or if it isn't imported or
/// fails to load. The player shows as vanilla for those few frames.
fn cobblemon(host: &dyn Host, species: &str) -> Option<&'static Cobbled> {
    enum Slot {
        Loading,
        Done(Option<&'static Cobbled>),
    }
    static LOADED: LazyLock<Mutex<HashMap<String, Slot>>> = LazyLock::new(Default::default);
    let mut loaded = LOADED.lock().unwrap();
    match loaded.get(species) {
        Some(Slot::Done(c)) => return *c,
        Some(Slot::Loading) => return None,
        None => {}
    }
    let Some(e) = cobblemon_index(host).get(species) else {
        loaded.insert(species.to_string(), Slot::Done(None));
        return None;
    };
    loaded.insert(species.to_string(), Slot::Loading);
    drop(loaded);
    let (dir, species, scale, idle, walk) = (content_folder(host), species.to_string(), e.scale, e.idle.clone(), e.walk.clone());
    let spawned = std::thread::Builder::new().name("capipimon model load".into()).spawn(move || {
        let read = |ext: &str| pack::read(&dir, &format!("cobblemon-models/{species}.{ext}.json"));
        let made = read("geo").zip(read("animation")).and_then(|(geo, anim)| {
            let creature = build(&geo, &anim, &idle, &walk)
                .map_err(|err| log::error!("capipimon_models: cobblemon {species} failed to load: {err}"))
                .ok()?;
            let c: &'static Cobbled = Box::leak(Box::new(Cobbled { sprite: format!("capipimon:entity/{species}"), scale, creature }));
            Some(c)
        });
        LOADED.lock().unwrap().insert(species, Slot::Done(made));
    });
    if let Err(err) = spawned {
        log::error!("capipimon_models: couldn't start a model loader: {err}");
    }
    None
}

/// Whether this game can draw `species`.
pub fn has_model(host: &dyn Host, species: &str) -> bool {
    cobblemon_index(host).contains_key(species)
}

/// The sprite, scale and model to draw `species` with.
fn model_of(host: &dyn Host, species: &str) -> Option<(&'static str, f32, &'static Creature)> {
    cobblemon(host, species).map(|c| (c.sprite.as_str(), c.scale, &c.creature))
}

/// Draw a creature into `draw`; returns the number of quads drawn.
fn draw_creature(
    host: &dyn Host,
    draw: &mut horizon_client::mod_api::BlockEntityDraw,
    (sprite, scale, c): (&str, f32, &Creature),
    anim: Option<&Animation>,
    time: f32,
    walking: bool,
    hat: Option<(&str, &str)>,
) -> usize {
    // Walking without a walk animation: the made-up gait over the idle, about 1.6 steps a second.
    let pose = if walking && c.walk.is_none() && !c.gait.is_empty() {
        c.model.pose_with(anim, time, &c.gait, (time * std::f32::consts::TAU * 1.6).sin())
    } else {
        c.model.pose(anim, time)
    };
    let mut drew = 0usize;
    c.model.emit(&pose, scale, |corners, uv| {
        if sprite_quad(host, draw, sprite, corners, uv) {
            drew += 1;
        }
    });
    if let Some((id, species)) = hat {
        cosmetics::draw::hat(host, draw, id, species, &c.model, &pose, scale);
    }
    drew
}

/// One textured quad of a creature. A `<species>_shiny` sprite that isn't in the content mod
/// falls back to the normal one, so any species can be shiny and only looks it once its shiny
/// texture is imported (`textures/entity/<species>_shiny.png`).
fn sprite_quad(host: &dyn Host, draw: &mut horizon_client::mod_api::BlockEntityDraw, sprite: &str, corners: [[f32; 3]; 4], uv: [f32; 4]) -> bool {
    if host.draw_quad(draw, sprite, corners, uv, 0xFF_FFFF) {
        return true;
    }
    // A palette skin that hasn't shipped yet is drawn as the line's plain shiny.
    let shiny = skin_base(sprite).map(|base| format!("{base}_shiny"));
    let sprite = shiny.as_deref().unwrap_or(sprite);
    (shiny.is_some() && host.draw_quad(draw, sprite, corners, uv, 0xFF_FFFF))
        || sprite.strip_suffix("_shiny").is_some_and(|normal| host.draw_quad(draw, normal, corners, uv, 0xFF_FFFF))
}

/// The species of a `<species>_p<tier>` palette skin sprite.
fn skin_base(sprite: &str) -> Option<&str> {
    let (base, tier) = sprite.rsplit_once("_p")?;
    tier.parse::<u32>().ok().map(|_| base)
}

/// The creature a player is drawn as on this client: only then is the vanilla name tag (at human
/// height) swapped for ours. Species without a model keep the player body and its tag.
fn drawn_as(host: &dyn Host, name: &str) -> bool {
    let Some(species) = play::CLIENT.lock().unwrap().species_by_name.get(name).cloned() else { return false };
    model_of(host, &species).is_some()
}

/// The battle box over a Pokémon player, in the main games' style, in place of the name tag:
/// a cream box with the name and level, and under them an HP bar (green, yellow under half, red
/// under a fifth). Just the name until the server's first numbers arrive.
///
/// Like vanilla's name tag it turns with the camera's whole rotation (yaw and pitch), so it lies
/// flat to the view from any angle. Every layer is drawn with the font: solid parts are the
/// font's full-block glyph scaled to size, because block-entity quads are shaded by which way they
/// face (an upright quad comes out grey, one seen from below dark) and text is not. Layers stand a
/// real distance apart toward the camera (more when far away, where depth precision is coarser),
/// so nothing flickers through what is in front of it.
fn draw_name_tag(host: &dyn Host, draw: &mut horizon_client::mod_api::BlockEntityDraw, name: &str, body_yaw: f32, top: f32) {
    let Some(cam) = host.camera() else { return };
    let plate = play::plate_of(name);
    let dist = host
        .client_entities()
        .into_iter()
        .find(|e| e.player_name.as_deref() == Some(name))
        .map_or(8.0, |e| {
            let d = [e.position[0] - cam.position[0], e.position[1] + f64::from(top) - cam.position[1], e.position[2] - cam.position[2]];
            (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() as f32
        });
    let label = play::plate_label(name);
    let title = play::onboard::launch_ui::title_of(name);
    let crew = play::onboard::crew::chip_of(name);
    draw_plate(host, draw, label.as_deref().unwrap_or(name), title.as_deref(), plate, crew.as_ref(), dist, body_yaw, top);
}

/// A real Pokémon NPC: its own model and animations, facing whoever it's fighting, with the
/// battle box players have over their heads.
fn draw_npc(host: &dyn Host, draw: &mut horizon_client::mod_api::BlockEntityDraw, mob: &horizon_client::mod_api::NativeMobRender<'_>, species: &str) -> Flow {
    let Some(m) = model_of(host, species) else { return Flow::Continue };
    let c = m.2;
    let time = mob.age / 20.0;
    let view = play::npc_view(mob.id);
    let key = format!("#{}", mob.id);
    let cast = play::cast_pose_of(&key, |w| standin::secs(true, w));
    let moving = view.as_ref().is_some_and(|v| v.moving);
    let named = cast.as_ref().and_then(|(a, ..)| c.extra.get(a));
    let standin = cast
        .as_ref()
        .filter(|(a, ..)| (a == "physical" || a == "special") && !c.extra.contains_key(a))
        .map(|(a, e, w)| (a == "special", *e, *w))
        .filter(|&(sp, e, w)| e < standin::secs(sp, w))
        .map(|(sp, e, w)| c.rig.pose(sp, standin::amount(sp, e, w)));
    let anim = match named {
        Some(a) => Some(a),
        None if moving => c.walk.as_ref().or(c.idle.as_ref()),
        None => c.idle.as_ref(),
    };
    let body = play::kit(species).map_or(1.0, |k| k.body);
    let size = play::npc_size(mob.id);
    draw.push();
    if size != 1.0 {
        draw.scale([size, size, size]);
    }
    if let Some(v) = &view {
        // The pose is turned with the carrier's body: undo that, then face where the server says.
        draw.rotate_about([0.0, 1.0, 0.0], mob.body_yaw - v.facing(), [0.0, 0.0, 0.0]);
    }
    if mob.death_time > 0 {
        // Fainting: tips over, as vanilla mobs do.
        let t = (mob.death_time as f32 / 20.0).min(1.0);
        draw.rotate_about([0.0, 0.0, 1.0], t * 90.0, [0.0, 0.0, 0.0]);
    }
    if let Some(p) = &standin {
        draw.translate([0.0, p.up * body, p.fwd * body]);
        draw.rotate_about([1.0, 0.0, 0.0], p.lean, [0.0, 0.0, 0.0]);
        if p.squash != 0.0 {
            draw.scale([1.0 - p.squash / 2.0, 1.0 + p.squash, 1.0 - p.squash]);
        }
        let pose = c.model.pose_with(c.idle.as_ref(), time, &p.offsets, 1.0);
        c.model.emit(&pose, m.1, |corners, uv| {
            sprite_quad(host, draw, m.0, corners, uv);
        });
    } else {
        draw_creature(host, draw, m, anim, time, moving && named.is_none(), None);
    }
    draw.pop();
    if let Some(v) = view.filter(|_| mob.death_time == 0) {
        let name = play::kit(species).map_or(species.to_string(), |k| k.name.clone());
        draw_plate(host, draw, &name, None, Some((v.level, v.hp, v.max)), None, 10.0, mob.body_yaw, body * size);
    }
    Flow::Cancel
}

/// The tag over a head: `plate` makes it a battle box, otherwise it's just the name. `crew` adds
/// the player's Crew chip (tag and banner shape in the banner's colours) to the row over the box.
#[allow(clippy::too_many_arguments)]
fn draw_plate(host: &dyn Host, draw: &mut horizon_client::mod_api::BlockEntityDraw, name: &str, title: Option<&str>, plate: Option<(u32, f32, f32)>, crew: Option<&play::onboard::crew::Chip>, dist: f32, body_yaw: f32, top: f32) {
    // Thunderstorm: only the Zapdos who called it sees nameplates (GDD 8.2).
    if play::in_storm() {
        return;
    }
    let Some(cam) = host.camera() else { return };
    // Far away it grows (up to 2.5x by 30 b) so it stays readable, as a nameplate in a PvP game
    // should; it is raised by what it grows so it never sinks into the head.
    let k = (dist / 12.0).clamp(1.0, 2.5);
    draw.push();
    draw.translate([0.0, top + 0.35 + 0.3 * (k - 1.0), 0.0]);
    // The pose is turned with the body (+z forward): undo that, then take the camera's rotation
    // (vanilla `EntityRenderDispatcher.cameraOrientation`: yaw π − yRot, then pitch −xRot).
    draw.rotate_about([0.0, 1.0, 0.0], body_yaw + 180.0 - cam.yaw, [0.0, 0.0, 0.0]);
    draw.rotate_about([1.0, 0.0, 0.0], -cam.pitch, [0.0, 0.0, 0.0]);
    draw.scale([k / 40.0, -k / 40.0, k / 40.0]);
    // One layer step toward the camera: 0.01 block up close, growing with distance (in units of
    // k/40 block, hence the / k).
    let step = 0.4 * (dist / 8.0).max(1.0) / k;
    let Some((level, hp, max)) = plate else {
        let named = crew.map(|c| format!("[{}] {name}", c.tag));
        let name = named.as_deref().unwrap_or(name);
        let w = host.text_width(name);
        host.draw_text(draw, &Text::literal(name.to_string()), [-w / 2.0, 0.0], 0xFFFFFF, true, 0x202020);
        if let Some(t) = title {
            let tw = host.text_width(t) * 0.75;
            draw.translate([-tw / 2.0, -9.0, 0.0]);
            draw.scale([0.75, 0.75, 1.0]);
            host.draw_text(draw, &Text::literal(t.to_string()), [0.0, 0.0], 0xFFD24A, true, 0x202020);
        }
        draw.pop();
        return;
    };
    let lv = format!("Lv{level}");
    let (nw, lw) = (host.text_width(name), host.text_width(&lv));
    let w = (nw + lw + 14.0).max(70.0);
    let (x0, y0, h) = (-w / 2.0, -14.0, 24.0);
    // A solid rectangle on layer `layer` (0 the back): the full-block glyph (8 × 8) stretched to
    // fit, drawn glowing (full bright, unshaded) so it keeps its colour at night and from below.
    let rect = |draw: &mut horizon_client::mod_api::BlockEntityDraw, x: f32, y: f32, rw: f32, rh: f32, rgb: u32, layer: f32| {
        if rw <= 0.0 || rh <= 0.0 {
            return;
        }
        draw.push();
        draw.translate([x, y, layer * step]);
        draw.scale([rw / 8.0, rh / 8.0, 1.0]);
        host.draw_text(draw, &Text::literal("\u{2588}".to_string()), [0.0, 0.0], rgb, true, 0);
        draw.pop();
    };
    let text = |draw: &mut horizon_client::mod_api::BlockEntityDraw, t: &str, x: f32, y: f32, rgb: u32, scale: f32, layer: f32| {
        draw.push();
        draw.translate([x, y, layer * step]);
        draw.scale([scale, scale, 1.0]);
        host.draw_text(draw, &Text::literal(t.to_string()), [0.0, 0.0], rgb, true, 0);
        draw.pop();
    };
    rect(draw, x0 - 1.0, y0 - 1.0, w + 2.0, h + 2.0, 0x404040, 0.0);
    rect(draw, x0, y0, w, h, 0xF8F8D8, 1.0);
    let frac = if max > 0.0 { (hp / max).clamp(0.0, 1.0) } else { 0.0 };
    let bar = if frac > 0.5 { 0x30D040 } else if frac > 0.2 { 0xF8C800 } else { 0xF83820 };
    let (bx, by, bw) = (x0 + 18.0, y0 + 14.0, w - 22.0);
    rect(draw, bx - 1.0, by - 1.0, bw + 2.0, 6.0, 0x404040, 2.0);
    rect(draw, x0 + 3.0, by - 2.0, 13.0, 8.0, 0x404040, 2.0);
    rect(draw, bx, by, bw, 4.0, 0x506858, 3.0);
    rect(draw, bx, by, bw * frac, 4.0, bar, 4.0);
    text(draw, name, x0 + 4.0, y0 + 3.0, 0x383838, 1.0, 5.0);
    text(draw, &lv, x0 + w - 4.0 - lw, y0 + 3.0, 0x383838, 1.0, 5.0);
    // "HP" in the games' amber on its dark tab.
    text(draw, "HP", x0 + 4.5, by - 1.0, 0xF8B830, 0.75, 5.0);
    // Signature statuses, and a transformed Ditto's marker, in a row over the box.
    let mut tags: Vec<(String, u32, u32)> = play::sig::statuses_of(name).into_iter().map(|(l, _, c)| (l.to_string(), c & 0xFF_FFFF, 0x101010)).collect();
    if play::sig::is_ditto(name) {
        tags.insert(0, ("DITTO".to_string(), 0xB070F0, 0x101010));
    }
    if let Some(c) = crew {
        tags.insert(0, (c.label(), c.rgb, c.trim));
    }
    if !tags.is_empty() {
        let widths: Vec<f32> = tags.iter().map(|(t, ..)| host.text_width(t) * 0.75 + 6.0).collect();
        let mut x = -widths.iter().sum::<f32>() / 2.0;
        for ((t, rgb, ink), tw) in tags.iter().zip(&widths) {
            rect(draw, x, y0 - 10.0, tw - 2.0, 8.0, *rgb, 1.0);
            text(draw, t, x + 2.0, y0 - 9.0, *ink, 0.75, 2.0);
            x += tw;
        }
    }
    // The title this player wears (`/title`), in gold on a dark tab over everything else.
    if let Some(t) = title {
        let tw = host.text_width(t) * 0.75;
        let y = y0 - if tags.is_empty() { 11.0 } else { 21.0 };
        rect(draw, -tw / 2.0 - 3.0, y, tw + 6.0, 9.0, 0x303038, 1.0);
        text(draw, t, -tw / 2.0, y + 1.5, 0xFFD24A, 0.75, 2.0);
    }
    draw.pop();
}

/// An egg (onboarding: Nest hatches and Hatchery pedestals): a lathe of quads in a light tint of
/// the species' type, painted with a plain white patch of Togepi's texture. `t` 0..1 runs the
/// hatch: two wobbles, then a swelling crack before the server's burst; `None` stands still.
fn draw_egg(host: &dyn Host, draw: &mut horizon_client::mod_api::BlockEntityDraw, rgb: u32, t: Option<f32>, size: f32) {
    let Some((sprite, _, _)) = model_of(host, "togepi") else { return };
    const UV: [f32; 4] = [6.75, 2.5, 7.75, 3.5];
    const RINGS: usize = 8;
    const SEGS: usize = 12;
    let (h, r0) = (0.9 * size, 0.36 * size);
    let tau = std::f32::consts::TAU;
    draw.push();
    if let Some(t) = t {
        // Wobbles at 0.1-0.4 and 0.45-0.75 of the hatch, then a quick swell.
        let wob = |a: f32, b: f32| if (a..b).contains(&t) { ((t - a) / (b - a) * tau).sin() } else { 0.0 };
        let tilt = 14.0 * wob(0.1, 0.4) - 14.0 * wob(0.45, 0.75);
        draw.rotate_about([0.0, 0.0, 1.0], tilt, [0.0, 0.0, 0.0]);
        if t > 0.8 {
            let k = 1.0 + 0.12 * ((t - 0.8) / 0.2);
            draw.scale([k, 1.0 / k.sqrt(), k]);
        }
    }
    let ring = |j: usize| {
        let s = j as f32 / RINGS as f32;
        let r = r0 * (std::f32::consts::PI * s).sin().powf(0.85) * (1.12 - 0.3 * s);
        (r, h * s)
    };
    for j in 0..RINGS {
        let (rl, yl) = ring(j);
        let (ru, yu) = ring(j + 1);
        for i in 0..SEGS {
            let (a0, a1) = (i as f32 / SEGS as f32 * tau, (i + 1) as f32 / SEGS as f32 * tau);
            let p = |r: f32, a: f32, y: f32| [r * a.cos(), y, r * a.sin()];
            // Top-left, bottom-left, bottom-right, top-right as seen from outside.
            let corners = [p(ru, a1, yu), p(rl, a1, yl), p(rl, a0, yl), p(ru, a0, yu)];
            host.draw_quad(draw, sprite, corners, UV, rgb);
        }
    }
    draw.pop();
}

/// Cancel a server-side player action when the acting player is a Pokémon.
#[cfg(feature = "capipimon_server")]
fn pokemon_acting(host: &dyn Host) -> Flow {
    match host.acting_player() {
        Some(p) if play::attack_by_pokemon(p.uuid) => Flow::Cancel,
        _ => Flow::Continue,
    }
}

impl Hooks for Mod {
    /// The move effects pass (`fx.rs`).
    fn render(&self) -> Option<&dyn RenderHooks> {
        Some(&*fx::RENDER)
    }

    /// Each frame: effects bookkeeping, and the camera flag the game-side helper (`frame.rs`)
    /// reads: third person while a Pokémon, the player's own view back afterwards.
    fn frame(&self, host: &mut dyn Host, _event: &mut FrameEvent) {
        fx::frame(host);
        cosmetics::install(&*host);
        // No player for 5 s of real time (single player closed its world, where
        // `client_disconnecting` doesn't fire): this client isn't a Pokémon any more either.
        // Real time, not frames, so a quick dimension change or respawn never trips it.
        static NO_PLAYER: Mutex<Option<std::time::Instant>> = Mutex::new(None);
        {
            let mut since = NO_PLAYER.lock().unwrap();
            if host.player().is_some() {
                *since = None;
            } else {
                let at = *since.get_or_insert_with(std::time::Instant::now);
                if at.elapsed().as_secs_f32() >= 5.0 && play::is_pokemon() {
                    play::forget();
                    fx::forget();
                }
            }
        }
        static VIEW: Mutex<Option<bool>> = Mutex::new(None);
        let mon = play::is_pokemon();
        let mut last = VIEW.lock().unwrap();
        if *last != Some(mon) {
            *last = Some(mon);
            // One key per process, so two game windows sharing one instance folder (the test
            // rig) don't overwrite each other's camera flag.
            let key = format!("view-{}", std::process::id());
            host.set_setting(MOD_ID, &key, if mon { "pokemon" } else { "trainer" });
        }
        drop(last);
        // How high this form steps; the game half reads it in frame.rs.
        static STEP: Mutex<f32> = Mutex::new(0.0);
        let want = play::step_height();
        let mut step = STEP.lock().unwrap();
        if *step != want {
            *step = want;
            host.set_setting(MOD_ID, &format!("step-{}", std::process::id()), &format!("{want:.1}"));
        }
    }

    /// Leaving a server: the Pokémon state, its camera and its blocked keys end with the
    /// connection (the server drops the Pokémon on logout too).
    fn client_disconnecting(&self, _host: &dyn Host, _event: &mut ClientDisconnectingEvent) {
        play::forget();
        fx::forget();
        emotes::forget();
    }

    fn camera_shake(&self, _host: &dyn Host, event: &mut CameraShakeEvent<'_>) {
        fx::camera_shake(event);
    }

    fn visuals(&self, _host: &dyn Host, event: &mut VisualsEvent<'_>) {
        fx::visuals(event);
    }

    /// `/capifx low|high`: the Low FX toggle (client only).
    fn command(&self, host: &mut dyn Host, event: &mut CommandEvent<'_>) -> Flow {
        if fx::command(host, event.command) || emotes::command(host, event.command) || cosmetics::draw::client_command(host, event.command) || play::onboard::guide::command(event.command) || play::onboard::photo::command(host, event.command) {
            Flow::Cancel
        } else {
            Flow::Continue
        }
    }

    fn movement_input(&self, _host: &dyn Host, event: &mut MovementInputEvent) {
        play::movement_input(event);
    }

    /// A Pokémon has no hotbar to scroll.
    fn wheel(&self, host: &mut dyn Host, _event: &mut WheelEvent) -> Flow {
        if host.playing() && play::is_pokemon() { Flow::Cancel } else { Flow::Continue }
    }

    /// Q is a move while a Pokémon, so it doesn't drop the held item (`remote_drop`, trait injection).
    fn inject_capipimon_models_drop(&self, _event: &mut InjectCapipimonModelsDropEvent) -> Flow {
        if play::is_pokemon() { Flow::Cancel } else { Flow::Continue }
    }

    /// F doesn't swap hands while a Pokémon (`remote_swap_hands`; the server refuses it too).
    fn inject_capipimon_models_swap_hands(&self, _event: &mut InjectCapipimonModelsSwapHandsEvent) -> Flow {
        if play::is_pokemon() { Flow::Cancel } else { Flow::Continue }
    }

    #[cfg(feature = "capipimon_server")]
    fn living_incoming_damage(&self, event: &mut LivingIncomingDamageEvent<'_>) -> Flow {
        if play::incoming_damage(event) { Flow::Cancel } else { Flow::Continue }
    }

    #[cfg(feature = "capipimon_server")]
    fn player_died(&self, _host: &mut dyn Host, event: &mut PlayerDiedEvent<'_>) {
        play::heard_died(event.player);
    }

    #[cfg(feature = "capipimon_server")]
    fn player_respawned(&self, _host: &mut dyn Host, event: &mut PlayerRespawnedEvent<'_>) {
        play::heard_respawned(event.player);
    }

    #[cfg(feature = "capipimon_server")]
    fn player_logged_in(&self, _host: &mut dyn Host, event: &mut PlayerLoggedInEvent<'_>) {
        play::heard_logged_in(event.player);
    }

    #[cfg(feature = "capipimon_server")]
    fn player_logged_out(&self, _host: &mut dyn Host, event: &mut PlayerLoggedOutEvent<'_>) {
        play::heard_logged_out(event.player);
    }

    /// Server: a Pokémon player's melee swing does nothing (only moves deal damage).
    #[cfg(feature = "capipimon_server")]
    fn attack_entity(&self, host: &mut dyn Host, _event: &mut AttackEntityEvent<'_>) -> Flow {
        match host.acting_player() {
            Some(p) if play::attack_by_pokemon(p.uuid) => Flow::Cancel,
            _ => Flow::Continue,
        }
    }

    /// Server: while a Pokémon, a player doesn't use blocks or items, break blocks or swap hands
    /// (GDD 3.1: those keys are moves, or nothing). A trainer is unaffected.
    #[cfg(feature = "capipimon_server")]
    fn right_click_block(&self, host: &mut dyn Host, _event: &mut RightClickBlockEvent) -> Flow {
        pokemon_acting(host)
    }

    #[cfg(feature = "capipimon_server")]
    fn use_item_on(&self, host: &mut dyn Host, _event: &mut UseItemOnEvent<'_>) -> Flow {
        pokemon_acting(host)
    }

    #[cfg(feature = "capipimon_server")]
    fn use_item(&self, host: &mut dyn Host, _event: &mut UseItemEvent<'_>) -> Flow {
        pokemon_acting(host)
    }

    #[cfg(feature = "capipimon_server")]
    fn left_click_block(&self, host: &mut dyn Host, _event: &mut LeftClickBlockEvent) -> Flow {
        pokemon_acting(host)
    }

    #[cfg(feature = "capipimon_server")]
    fn player_swap_hands(&self, host: &mut dyn Host, _event: &mut PlayerSwapHandsEvent<'_>) -> Flow {
        pokemon_acting(host)
    }

    /// Server: a Pokémon's hotbar slot stays put (the client pins it too; this catches a key
    /// that got through first: the server sends the old slot back).
    #[cfg(feature = "capipimon_server")]
    fn player_item_held(&self, _host: &mut dyn Host, event: &mut PlayerItemHeldEvent<'_>) -> Flow {
        if play::pokemon_player(event.player) { Flow::Cancel } else { Flow::Continue }
    }

    /// Server: no pick block (middle click) while a Pokémon.
    #[cfg(feature = "capipimon_server")]
    fn player_pick_item(&self, _host: &mut dyn Host, event: &mut PlayerPickItemEvent<'_>) -> Flow {
        if play::pokemon_player(event.player) { Flow::Cancel } else { Flow::Continue }
    }

    /// The attack key (left click) is a Pokémon's basic attack (slot 1), not a punch or a block break.
    /// Fires again each tick it's held on a block; the move's cooldown paces that.
    fn interaction_key(&self, host: &mut dyn Host, event: &mut InteractionKeyEvent<'_>) -> Flow {
        if event.button == 0 && play::left_click(host) {
            event.swing_hand = false;
            Flow::Cancel
        } else if event.button != 0 && play::is_pokemon() {
            event.swing_hand = false;
            Flow::Cancel
        } else {
            Flow::Continue
        }
    }

    fn camera_pose(&self, host: &dyn Host, event: &mut CameraPoseEvent<'_>) {
        play::camera_pose(host, event);
        // Photo mode holds the camera where it was when the photo started.
        play::onboard::photo::camera_pose(event);
    }

    /// `/mute`: chat from players this client muted isn't shown. Server messages always are.
    fn client_chat_received(&self, _host: &mut dyn Host, event: &mut horizon_client::mod_api::ClientChatReceivedEvent<'_>) -> Flow {
        if !event.system && play::onboard::launch_ui::is_muted(event.sender) { Flow::Cancel } else { Flow::Continue }
    }

    /// No human arm (or held item) in first person while playing as a Pokémon or spectating after a KO.
    /// (`render_hand_pose` can't do it: Horizon resets a scale of 0 to 1.)
    fn render_hand(&self, _host: &dyn Host, _event: &mut RenderHandEvent<'_>) -> Flow {
        let c = play::CLIENT.lock().unwrap();
        if c.me.is_some() || c.ko.is_some() { Flow::Cancel } else { Flow::Continue }
    }

    /// Hide the vanilla tag of players drawn as a creature; `player_submit` draws ours.
    fn render_name_tag(&self, host: &dyn Host, event: &mut RenderNameTagEvent<'_>) {
        // Carriers (wilds, foes, pedestals, the Meadow's Wooloo, dummies) are named only so
        // `mob_submit` knows what to draw; vanilla would show that under the crosshair.
        if event.kind.starts_with("capipimon:") {
            event.visible = false;
            return;
        }
        if event.kind != "minecraft:player" || !event.visible {
            return;
        }
        let Some(name) = host.client_entities().into_iter().find(|e| e.id == event.entity_id).and_then(|e| e.player_name) else {
            return;
        };
        if drawn_as(host, &name) {
            event.visible = false;
        }
    }

    fn player_render_listened(&self, _host: &dyn Host, event: &mut PlayerRenderListenedEvent) {
        event.listened = true;
    }

    /// A player who has become a Pokémon is drawn as that species.
    fn player_submit(&self, host: &dyn Host, event: &mut PlayerSubmitEvent<'_>) -> Flow {
        // Hatching (onboarding): an egg where the player stands, in place of any body.
        if let Some((species, t)) = play::onboard::hatching_of(event.who.name) {
            draw_egg(host, &mut *event.draw, play::onboard::egg_tint(&species), Some(t), 1.0);
            return Flow::Cancel;
        }
        // Caught in a capsule (GDD 6.2): a red-and-white ball while it struggles.
        if play::onboard::catch::balled(event.who.name) {
            draw_egg(host, &mut *event.draw, 0xE0_3030, None, 0.8);
            return Flow::Cancel;
        }
        let Some((species, cast)) = play::look_of(event.who.name, event.who.local) else { return Flow::Continue };
        // Photo mode: this client's own Pokémon holds the pose it picked (nobody else sees it).
        let photo_pose = if event.who.local { play::onboard::photo::pose() } else { None };
        let cast = if photo_pose.is_some() { None } else { cast };
        let Some(m) = model_of(host, &species) else { return Flow::Continue };
        // Shiny (`/capi become <species> shiny`, `/capi shiny on`): the `_shiny` texture.
        let shiny_sprite = play::shiny_of(event.who.name).then(|| match play::skin_of(event.who.name) {
            0 => format!("{}_shiny", m.0),
            tier => format!("{}_p{tier}", m.0),
        });
        let m: (&str, f32, &Creature) = (shiny_sprite.as_deref().unwrap_or(m.0), m.1, m.2);
        let c = m.2;
        let mob = event.player;
        let anim = match cast.as_deref().and_then(|a| c.extra.get(a)) {
            Some(a) => Some(a),
            None if mob.walk_speed > 0.1 => c.walk.as_ref().or(c.idle.as_ref()),
            None => c.idle.as_ref(),
        };
        // A dodge roll: one full tumble that way, about the middle of the body.
        let roll = play::roll_of(event.who.name);
        if let Some((code, t)) = roll {
            let mid = play::kit(&species).map_or(0.5, |k| k.body * 0.5);
            let e = 1.0 - (1.0 - t) * (1.0 - t);
            let (axis, sign) = match code {
                'b' => ([1.0, 0.0, 0.0], -1.0),
                'l' => ([0.0, 0.0, 1.0], -1.0),
                'r' => ([0.0, 0.0, 1.0], 1.0),
                _ => ([1.0, 0.0, 0.0], 1.0),
            };
            event.draw.push();
            event.draw.rotate_about(axis, sign * 360.0 * e, [0.0, mid, 0.0]);
        }
        // A cast the model has no animation for gets a made-up pose for its body plan (standin.rs).
        let standin = play::cast_pose_of(event.who.name, |w| standin::secs(true, w))
            .filter(|_| photo_pose.is_none())
            .filter(|(a, ..)| (a == "physical" || a == "special") && !c.extra.contains_key(a))
            .map(|(a, e, w)| (a == "special", e, w))
            .filter(|&(sp, e, w)| e < standin::secs(sp, w))
            .map(|(sp, e, w)| {
                let a = standin::amount(sp, e, w);
                (c.rig.pose(sp, a), standin::tremble(sp, a, mob.age / 20.0))
            });
        // A photo pose: the body plan's attack pose, held at its peak.
        let standin = match photo_pose {
            Some(special) => Some((c.rig.pose(special, 1.0), 0.0)),
            None => standin,
        };
        let walking = cast.is_none() && standin.is_none() && mob.walk_speed > 0.1;
        // An emote (emotes.rs), only while no cast, wind-up or roll is playing: its animation if
        // the model has one, else a made-up motion of the whole body.
        let emote = (cast.is_none() && standin.is_none() && roll.is_none()).then(|| emotes::playing(event.who.name, mob.walk_speed > 0.1, event.who.local)).flatten();
        let emote_anim = emote.as_ref().and_then(|e| emotes::pick_anim(e.anims, |a| c.extra.contains_key(a))).and_then(|a| c.extra.get(a));
        let emote_motion = emote.as_ref().filter(|e| emote_anim.is_none() || (e.looping && !emote_anim.is_some_and(|a| a.looping))).map(|e| emotes::motion(e.motion, e.t, e.secs));
        // What this player wears (visual only: nothing here reaches hitboxes or moves).
        let worn = cosmetics::draw::worn_by(event.who.name, event.who.local);
        let hat = worn.as_ref().and_then(|w| w.hat.as_deref()).map(|id| (id, species.as_str()));
        // Minimize: drawn smaller, standing on the same spot.
        let size = play::size_of(event.who.name);
        if size != 1.0 {
            event.draw.push();
            event.draw.scale([size, size, size]);
        }
        let drawn = if let Some((p, shake)) = &standin {
            // Lunge / rear up: the whole body moves, leans and squashes about its feet (+z is
            // forward); a held special trembles.
            let body = play::kit(&species).map_or(1.0, |k| k.body);
            event.draw.push();
            event.draw.translate([0.0, p.up * body, p.fwd * body]);
            event.draw.rotate_about([1.0, 0.0, 0.0], p.lean, [0.0, 0.0, 0.0]);
            if *shake != 0.0 {
                event.draw.rotate_about([0.0, 0.0, 1.0], *shake, [0.0, 0.0, 0.0]);
            }
            if p.squash != 0.0 {
                event.draw.scale([1.0 - p.squash / 2.0, 1.0 + p.squash, 1.0 - p.squash]);
            }
            let pose = c.model.pose_with(c.idle.as_ref(), mob.age / 20.0, &p.offsets, 1.0);
            let mut drew = 0usize;
            c.model.emit(&pose, m.1, |corners, uv| {
                if sprite_quad(host, &mut *event.draw, m.0, corners, uv) {
                    drew += 1;
                }
            });
            if let Some((id, sp)) = hat {
                cosmetics::draw::hat(host, &mut *event.draw, id, sp, &c.model, &pose, m.1);
            }
            event.draw.pop();
            drew
        } else if let (Some(e), Some(a)) = (&emote, emote_anim) {
            if let Some(mo) = emote_motion {
                emotes::push_motion(&mut *event.draw, mo, play::kit(&species).map_or(1.0, |k| k.body));
            }
            let drew = draw_creature(host, &mut *event.draw, m, Some(a), e.t, false, hat);
            if emote_motion.is_some() {
                event.draw.pop();
            }
            drew
        } else if let Some(mo) = emote_motion {
            emotes::push_motion(&mut *event.draw, mo, play::kit(&species).map_or(1.0, |k| k.body));
            let drew = draw_creature(host, &mut *event.draw, m, anim, mob.age / 20.0, walking, hat);
            event.draw.pop();
            drew
        } else {
            draw_creature(host, &mut *event.draw, m, anim, mob.age / 20.0, walking, hat)
        };
        if size != 1.0 {
            event.draw.pop();
        }
        if roll.is_some() {
            event.draw.pop();
        }
        if drawn == 0 {
            return Flow::Continue;
        }
        let (height, radius) = (play::kit(&species).map_or(1.0, |k| k.body) * size, c.radius * m.1 * size);
        if let Some(w) = &worn {
            cosmetics::draw::particles(host, &mut *event.draw, w, event.who.name, height, radius, mob, true);
        }
        // Double Team: copies to the left, right and behind, a little out of step with it. They
        // wear what the real one wears, or the cosmetics would give the trick away.
        let copies = play::copies_of(event.who.name);
        if copies > 0 {
            let w = play::kit(&species).map_or(1.0, |k| k.body).clamp(0.8, 3.0) * 1.3;
            let spots = [[-w, 0.0, -0.3 * w], [w, 0.0, -0.3 * w], [0.0, 0.0, -1.1 * w]];
            for (i, at) in spots.iter().take(usize::from(copies)).enumerate() {
                event.draw.push();
                event.draw.translate(*at);
                event.draw.scale([size, size, size]);
                draw_creature(host, &mut *event.draw, m, anim, mob.age / 20.0 + 0.37 * (i + 1) as f32, walking, hat);
                event.draw.pop();
                if let Some(w) = &worn {
                    event.draw.push();
                    event.draw.translate(*at);
                    cosmetics::draw::particles(host, &mut *event.draw, w, event.who.name, height, radius, mob, false);
                    event.draw.pop();
                }
            }
        }
        let top = play::kit(&species).map_or(1.0, |k| k.body) * size;
        if !event.who.local {
            draw_name_tag(host, &mut *event.draw, event.who.name, mob.body_yaw, top);
        }
        // The emote's bubble, over the nameplate (no plate over your own head).
        if let Some(e) = &emote {
            let over = if event.who.local { 0.45 } else { 1.15 };
            emotes::bubble(host, &mut *event.draw, event.who.name, e, top + over, mob.body_yaw);
        }
        Flow::Cancel
    }

    /// A trainer (drawn as a plain player): an emote's bubble over the name tag.
    fn mob_layer(&self, host: &dyn Host, event: &mut MobLayerEvent<'_>) {
        let Some(who) = event.who else { return };
        if let Some(e) = emotes::playing(who.name, event.mob.walk_speed > 0.1, who.local) {
            let over = if who.local { 2.25 } else { 2.6 };
            emotes::bubble(host, &mut *event.draw, who.name, &e, over, event.mob.body_yaw);
        }
    }

    #[cfg(feature = "capipimon_server")]
    fn player_command(&self, _host: &mut dyn Host, event: &mut horizon_client::mod_api::PlayerCommandEvent<'_>) -> Flow {
        if play::automod::command(event.player, event.name, event.command, event.replies) { Flow::Cancel } else { Flow::Continue }
    }

    #[cfg(feature = "capipimon_server")]
    fn player_chat(&self, event: &mut PlayerChatEvent<'_>) -> Flow {
        if play::automod::chat(event) { Flow::Cancel } else { Flow::Continue }
    }

    #[cfg(feature = "capipimon_server")]
    fn register_commands(&self, host: &mut dyn Host, event: &mut RegisterCommandsEvent<'_>) {
        play::register_commands(host, event);
    }

    #[cfg(feature = "capipimon_server")]
    fn perform_command(&self, host: &mut dyn Host, event: &mut PerformCommandEvent<'_>) -> Flow {
        if play::perform_command(host, event) { Flow::Cancel } else { Flow::Continue }
    }

    #[cfg(feature = "capipimon_server")]
    fn net_from_client(&self, _host: &mut dyn Host, event: &mut NetFromClientEvent<'_>) {
        play::net_from_client(event);
    }

    fn net_from_server(&self, host: &mut dyn Host, event: &mut NetFromServerEvent<'_>) {
        play::net_from_server(host, event);
    }

    #[cfg(feature = "capipimon_server")]
    fn server_tick(&self, host: &mut dyn Host, event: &mut ServerTickEvent<'_>) {
        play::server_tick(host, event);
    }

    fn key(&self, host: &mut dyn Host, event: &mut KeyEvent<'_>) {
        play::key(host, event);
    }

    fn hud(&self, host: &mut dyn Host, event: &mut HudEvent<'_>) {
        play::hud(host, event);
    }

    /// A Pokémon in third person: the view and the feet for the over-the-shoulder camera
    /// (`play::camera_pose`). Only called while the camera is detached.
    fn camera_distance(&self, _host: &dyn Host, event: &mut CameraDistanceEvent<'_>) {
        let mon = play::CLIENT.lock().unwrap().me.is_some();
        if mon {
            play::camera_distance(event);
        }
    }

    /// A Pokémon has no hearts, hunger, XP or hotbar: the Capipimon HUD (`play::hud`) shows HP,
    /// energy and moves instead. Crosshair, effects, boss bars, chat, titles and the tab list stay.
    /// No tutorial hints (punch a tree...), advancement or recipe toasts, for everyone.
    fn toast_add(&self, _host: &mut dyn Host, event: &mut ToastAddEvent<'_>) -> Flow {
        match event.kind {
            "tutorial" | "advancement" | "recipe" => Flow::Cancel,
            _ => Flow::Continue,
        }
    }

    fn gui_layer_pre(&self, _host: &mut dyn Host, event: &mut GuiLayerPreEvent<'_>) -> Flow {
        const HIDDEN: [&str; 5] = [
            "minecraft:hotbar",
            "minecraft:player_health",
            "minecraft:contextual_info_bar",
            "minecraft:selected_item_name",
            "minecraft:vehicle_health",
        ];
        // Photo mode: a clean screen, whatever the layer.
        if play::onboard::photo::on() {
            return Flow::Cancel;
        }
        if HIDDEN.contains(&event.layer) && play::is_pokemon() { Flow::Cancel } else { Flow::Continue }
    }

    fn mob_submit(&self, host: &dyn Host, event: &mut MobSubmitEvent<'_>) -> Flow {
        let mob = event.mob;
        // Onboarding carriers named `capi:<species>`: a Hatchery pedestal (an egg with the
        // species turning over it) or the Meadow's Wild Wooloo (just the species).
        if let Some(species) = mob.custom_name.and_then(|n| n.trim().strip_prefix("capi:")) {
            if let Some(species) = species.strip_prefix("npc:") {
                return draw_npc(host, &mut *event.draw, mob, species);
            }
            let Some(m) = model_of(host, species) else { return Flow::Continue };
            let c = m.2;
            let time = mob.age / 20.0;
            let draw = &mut *event.draw;
            {
                draw_egg(host, draw, play::onboard::egg_tint(species), None, 0.7);
                // The hologram: about a block tall, turning slowly, over the egg.
                let body = play::kit(species).map_or(1.0, |k| k.body).max(0.4);
                let k = (1.0 / body).min(1.2);
                draw.push();
                draw.translate([0.0, 0.75, 0.0]);
                draw.rotate_about([0.0, 1.0, 0.0], time * 45.0, [0.0, 0.0, 0.0]);
                draw.scale([k, k, k]);
                draw_creature(host, draw, m, c.idle.as_ref(), time, false, None);
                draw.pop();
                let mark = if species == "buneary" { Some(("\u{2665}", 0xFF7FBF)) } else if play::onboard::featured(species) { Some(("\u{2605}", 0xFFD700)) } else { None };
                if let (Some((g, rgb)), Some(cam)) = (mark, host.camera()) {
                    draw.push();
                    draw.translate([0.0, 0.85 + body * k, 0.0]);
                    draw.rotate_about([0.0, 1.0, 0.0], mob.body_yaw + 180.0 - cam.yaw, [0.0, 0.0, 0.0]);
                    draw.scale([1.0 / 32.0, -1.0 / 32.0, 1.0 / 32.0]);
                    let w = host.text_width(g);
                    host.draw_text(draw, &Text::literal(g.to_string()), [-w / 2.0, 0.0], rgb, true, 0);
                    draw.pop();
                }
            }
            return Flow::Cancel;
        }
        let Some((sprite, scale, c)) = mob.kind.strip_prefix("capipimon:").and_then(|species| model_of(host, species)) else {
            return Flow::Continue;
        };
        let anim = if mob.walk_speed > 0.1 { c.walk.as_ref().or(c.idle.as_ref()) } else { c.idle.as_ref() };
        let time = mob.age / 20.0;
        let pose = if mob.walk_speed > 0.1 && c.walk.is_none() && !c.gait.is_empty() {
            c.model.pose_with(anim, time, &c.gait, (time * std::f32::consts::TAU * 1.6).sin())
        } else {
            c.model.pose(anim, time)
        };
        let mut drew = 0usize;
        let draw = &mut *event.draw;
        c.model.emit(&pose, scale, |corners, uv| {
            if host.draw_quad(draw, sprite, corners, uv, 0xFF_FFFF) {
                drew += 1;
            }
        });
        static TOLD: AtomicBool = AtomicBool::new(false);
        if !TOLD.swap(true, Ordering::Relaxed) {
            log::info!("capipimon_models: drew {} for mob {}: {drew} quads", mob.kind, mob.id);
        }
        if drew == 0 {
            static MISSING: AtomicBool = AtomicBool::new(false);
            if !MISSING.swap(true, Ordering::Relaxed) {
                log::warn!("capipimon_models: sprite {sprite} is not in the entity atlas; drawing the vanilla model");
            }
            return Flow::Continue;
        }
        Flow::Cancel
    }
}
