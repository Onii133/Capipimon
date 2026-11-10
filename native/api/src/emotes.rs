//! Emotes, the client half: the emote wheel on B, the speech bubble over a player's head and the
//! body motion a Pokémon plays. Free for everyone; nothing here is ever sold or locked.
//!
//! The server (`emotes_srv.rs`) decides who may emote and tells everyone in range. The only link
//! between the halves is the wire on `play::CHANNEL`:
//! - client to server: `emote\t<id>` (the wheel; `/emote <id>` goes through the server's command)
//! - server to client: `emote\t<name>\t<id>`, `emote\t<name>\t-` (a looping one ended) and
//!   `emote\t<name>\thighfive\t<other>`.
//!
//! The emote list is `data/emotes.json`. Each emote names the model animations to try in order
//! (`cry` is on 792 of the 989 Cobblemon models, `sleep` on 533, `happy` on about 210), then a
//! made-up whole-body motion for models with none of them. A cast, wind-up or dodge roll always
//! wins over an emote (`lib.rs` only asks for one when none of those is playing), so an emote
//! never hides a move's warning.

use horizon_client::mod_api::{BlockEntityDraw, Host, HudBatch, Text};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

/// How long the wheel stays open without a pick.
const WHEEL_SECS: f32 = 5.0;
/// A looping emote (dance, sleep) is dropped after this long even if the server never says so.
const LOOP_MAX_SECS: f32 = 120.0;
/// How long a high five shows.
const HIGHFIVE_SECS: f32 = 2.0;

#[derive(Deserialize, Clone, Debug)]
pub struct Emote {
    pub id: String,
    pub label: String,
    pub bubble: String,
    #[serde(default)]
    pub anims: Vec<String>,
    pub motion: String,
    #[serde(default)]
    pub secs: f32,
    #[serde(default, rename = "loop")]
    pub looping: bool,
}

#[derive(Deserialize, Clone, Debug)]
pub struct HighFive {
    pub label: String,
    pub bubble: String,
}

#[derive(Deserialize, Debug)]
pub struct Catalog {
    pub emotes: Vec<Emote>,
    pub highfive: HighFive,
}

pub static CATALOG: LazyLock<Catalog> =
    LazyLock::new(|| serde_json::from_str(include_str!("../data/emotes.json")).expect("data/emotes.json"));

pub fn find(id: &str) -> Option<&'static Emote> {
    CATALOG.emotes.iter().find(|e| e.id == id)
}

/// What one player is playing now.
struct Playing {
    id: String,
    at: Instant,
}

#[derive(Default)]
struct State {
    wheel: Option<Instant>,
    playing: HashMap<String, Playing>,
    /// `/emote hide`: other players' emotes aren't drawn (your own still are).
    hide_others: bool,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Default::default);

/// The first of `anims` the model has, or None (then the made-up motion plays).
pub fn pick_anim<'a>(anims: &'a [String], has: impl Fn(&str) -> bool) -> Option<&'a str> {
    anims.iter().map(String::as_str).find(|a| has(a))
}

/// A digit key's emote while the wheel is open: 1 is the first on the wheel.
pub fn wheel_pick(key: &str) -> Option<&'static Emote> {
    let d: usize = key.strip_prefix("key.keyboard.")?.parse().ok()?;
    d.checked_sub(1).and_then(|i| CATALOG.emotes.get(i))
}

/// Keys, before any other Capipimon key handling. True when taken. `busy`: another screen (PC,
/// wardrobe) is open, so B doesn't open the wheel over it.
pub fn key(host: &mut dyn Host, key: &str, busy: bool) -> bool {
    let mut s = STATE.lock().unwrap();
    if s.wheel.is_some_and(|at| at.elapsed().as_secs_f32() >= WHEEL_SECS) {
        s.wheel = None;
    }
    if s.wheel.is_none() {
        if key == "key.keyboard.b" && !busy {
            s.wheel = Some(Instant::now());
            return true;
        }
        return false;
    }
    match key {
        "key.keyboard.b" | "key.keyboard.escape" => {
            s.wheel = None;
            true
        }
        // A move closes the wheel and still goes out.
        "key.keyboard.q" | "key.keyboard.e" | "key.keyboard.r" | "key.keyboard.c" | "key.keyboard.f" => {
            s.wheel = None;
            false
        }
        _ => match wheel_pick(key) {
            Some(e) => {
                s.wheel = None;
                drop(s);
                let _ = host.net_send_to_server(crate::play::CHANNEL, format!("emote\t{}", e.id).as_bytes());
                true
            }
            // Movement, jump and guard keep working with the wheel open.
            None => false,
        },
    }
}

/// `/emote hide` and `/emote show` stay on this client; anything else goes to the server.
pub fn command(host: &mut dyn Host, command: &str) -> bool {
    let words: Vec<&str> = command.trim_start_matches('/').split_whitespace().collect();
    if words.first() != Some(&"emote") {
        return false;
    }
    let hide = match words.get(1).copied() {
        Some("hide") => true,
        Some("show") => false,
        _ => return false,
    };
    STATE.lock().unwrap().hide_others = hide;
    let msg = if hide { "Other players' emotes are hidden. /emote show brings them back." } else { "Showing other players' emotes." };
    host.client_action_bar(Text::literal(msg.to_string()), false);
    true
}

/// A server line; true when it was an emote line.
pub fn heard(f: &[&str]) -> bool {
    let mut s = STATE.lock().unwrap();
    match f {
        ["emote", name, "-"] => {
            s.playing.remove(*name);
        }
        ["emote", name, "highfive", _other] => {
            s.playing.insert(name.to_string(), Playing { id: "highfive".to_string(), at: Instant::now() });
        }
        ["emote", name, id] if find(id).is_some() => {
            s.playing.insert(name.to_string(), Playing { id: id.to_string(), at: Instant::now() });
        }
        ["emote", ..] => {}
        _ => return false,
    }
    true
}

/// Leaving a server: nobody is emoting any more.
pub fn forget() {
    let mut s = STATE.lock().unwrap();
    s.playing.clear();
    s.wheel = None;
}

/// An emote being played: what to show and for how long it has run.
pub struct Now {
    pub bubble: &'static str,
    /// The model animations to try, in order.
    pub anims: &'static [String],
    pub motion: &'static str,
    pub looping: bool,
    /// Seconds since it started.
    pub t: f32,
    /// How long it lasts (0 for a loop).
    pub secs: f32,
}

/// Seconds an emote runs for, or None while a loop runs.
fn length(id: &str) -> Option<f32> {
    if id == "highfive" {
        return Some(HIGHFIVE_SECS);
    }
    let e = find(id)?;
    (!e.looping).then_some(e.secs)
}

/// What `name` is playing. `walking`: a moving player's loop ends here at once, without waiting
/// for the server's word. `local`: this window's own player (shown even with `/emote hide`).
pub fn playing(name: &str, walking: bool, local: bool) -> Option<Now> {
    let mut s = STATE.lock().unwrap();
    if s.hide_others && !local {
        return None;
    }
    let p = s.playing.get(name)?;
    let t = p.at.elapsed().as_secs_f32();
    let over = match length(&p.id) {
        Some(secs) => t >= secs,
        None => t >= LOOP_MAX_SECS || (walking && t > 0.3),
    };
    if over {
        s.playing.remove(name);
        return None;
    }
    static NONE: Vec<String> = Vec::new();
    if p.id == "highfive" {
        return Some(Now { bubble: &CATALOG.highfive.bubble, anims: &NONE, motion: "hop", looping: false, t, secs: HIGHFIVE_SECS });
    }
    let e = find(&p.id)?;
    Some(Now { bubble: &e.bubble, anims: &e.anims, motion: &e.motion, looping: e.looping, t, secs: e.secs })
}

/// A whole-body move, in the body's frame (+z forward), for a model with none of the emote's
/// animations. Lengths are in body heights.
#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub struct Motion {
    pub up: f32,
    pub fwd: f32,
    /// Degrees forward (+) or back (-), about the feet.
    pub lean: f32,
    /// Degrees to the side, about the feet.
    pub roll: f32,
    /// Degrees turned about the vertical.
    pub yaw: f32,
    /// Squashed down (+) or stretched (-), as a share of the height.
    pub squash: f32,
}

/// 0 to 1 and back over `secs`: in quickly, held, out quickly.
fn bump(t: f32, secs: f32) -> f32 {
    if secs <= 0.0 {
        return 1.0;
    }
    let edge = (secs * 0.2).min(0.3);
    (t / edge).min((secs - t) / edge).clamp(0.0, 1.0)
}

pub fn motion(kind: &str, t: f32, secs: f32) -> Motion {
    use std::f32::consts::TAU;
    let b = bump(t, secs);
    let mut m = Motion::default();
    match kind {
        // Two little hops.
        "hop" => m.up = 0.18 * (t * TAU).sin().max(0.0) * b,
        "bow" => {
            m.lean = 28.0 * b;
            m.fwd = 0.05 * b;
        }
        "sway" => m.roll = 12.0 * (t * TAU * 1.2).sin() * b,
        "shake" => {
            m.yaw = 12.0 * (t * TAU * 4.0).sin() * b;
            m.squash = 0.05 * (t * TAU * 4.0).cos().abs() * b;
        }
        "rear" => {
            m.lean = -22.0 * b;
            m.up = 0.06 * b;
            m.squash = -0.06 * b;
        }
        "tilt" => m.roll = 18.0 * b,
        // Bounce and sway from side to side, two beats a second.
        "dance" => {
            m.up = 0.12 * (t * TAU * 2.0).sin().abs();
            m.yaw = 25.0 * (t * TAU * 1.0).sin();
            m.roll = 6.0 * (t * TAU * 1.0).sin();
        }
        // Lie low and breathe slowly.
        "lie" => {
            let settle = (t / 0.5).min(1.0);
            m.squash = (0.4 + 0.03 * (t * TAU * 0.4).sin()) * settle;
            m.lean = 8.0 * settle;
        }
        _ => {}
    }
    m
}

/// Apply `m` to `draw` for a body `body` blocks tall. Pair with one `draw.pop()`.
pub fn push_motion(draw: &mut BlockEntityDraw, m: Motion, body: f32) {
    draw.push();
    draw.translate([0.0, m.up * body, m.fwd * body]);
    if m.yaw != 0.0 {
        draw.rotate_about([0.0, 1.0, 0.0], m.yaw, [0.0, 0.0, 0.0]);
    }
    if m.lean != 0.0 {
        draw.rotate_about([1.0, 0.0, 0.0], m.lean, [0.0, 0.0, 0.0]);
    }
    if m.roll != 0.0 {
        draw.rotate_about([0.0, 0.0, 1.0], m.roll, [0.0, 0.0, 0.0]);
    }
    if m.squash != 0.0 {
        draw.scale([1.0 + m.squash / 3.0, 1.0 - m.squash, 1.0 + m.squash / 3.0]);
    }
}

/// The speech bubble over `name`, `top` blocks over its feet (just over the nameplate). Turned
/// to face the camera like the nameplate; it pops in over its first 0.15 s.
pub fn bubble(host: &dyn Host, draw: &mut BlockEntityDraw, name: &str, now: &Now, top: f32, body_yaw: f32) {
    if crate::play::in_storm() {
        return;
    }
    let Some(cam) = host.camera() else { return };
    let dist = host
        .client_entities()
        .into_iter()
        .find(|e| e.player_name.as_deref() == Some(name))
        // Not in the list = this window's own body, right by the camera.
        .map_or(3.0, |e| {
            let d = [e.position[0] - cam.position[0], e.position[1] + f64::from(top) - cam.position[1], e.position[2] - cam.position[2]];
            (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() as f32
        });
    if dist > 48.0 {
        return;
    }
    // Smaller up close (your own third-person body), bigger far off.
    let k = 1.3 * (dist / 10.0).clamp(0.35, 2.5) * (now.t / 0.15).clamp(0.2, 1.0);
    // A loop's bubble bobs gently.
    let bob = if now.looping { 0.05 * (now.t * 2.5).sin() } else { 0.0 };
    draw.push();
    draw.translate([0.0, top + bob + 0.3 * ((dist / 12.0).clamp(1.0, 2.5) - 1.0), 0.0]);
    draw.rotate_about([0.0, 1.0, 0.0], body_yaw + 180.0 - cam.yaw, [0.0, 0.0, 0.0]);
    draw.rotate_about([1.0, 0.0, 0.0], -cam.pitch, [0.0, 0.0, 0.0]);
    draw.scale([k / 40.0, -k / 40.0, k / 40.0]);
    let step = 0.4 * (dist / 8.0).max(1.0) / k.max(0.2);
    let w = host.text_width(now.bubble).max(8.0);
    let (x0, y0, bw, bh) = (-w / 2.0 - 4.0, -12.0, w + 8.0, 13.0);
    // Solid areas are the full-block glyph stretched, drawn glowing (see `draw_plate`).
    let rect = |draw: &mut BlockEntityDraw, x: f32, y: f32, rw: f32, rh: f32, rgb: u32, layer: f32| {
        draw.push();
        draw.translate([x, y, layer * step]);
        draw.scale([rw / 8.0, rh / 8.0, 1.0]);
        host.draw_text(draw, &Text::literal("\u{2588}".to_string()), [0.0, 0.0], rgb, true, 0);
        draw.pop();
    };
    rect(draw, x0 - 1.0, y0 - 1.0, bw + 2.0, bh + 2.0, 0x303030, 0.0);
    rect(draw, x0, y0, bw, bh, 0xFFFFFF, 1.0);
    // The tail, two steps down toward the head.
    rect(draw, -2.0, y0 + bh, 4.0, 2.0, 0xFFFFFF, 1.0);
    rect(draw, -1.0, y0 + bh + 2.0, 2.0, 2.0, 0xFFFFFF, 1.0);
    draw.push();
    draw.translate([-w / 2.0, y0 + 3.0, 2.0 * step]);
    host.draw_text(draw, &Text::literal(now.bubble.to_string()), [0.0, 0.0], 0x303030, false, 0);
    draw.pop();
    draw.pop();
}

/// The wheel, over everything else in the HUD while open.
pub fn hud(host: &mut dyn Host, b: &mut HudBatch) {
    let open = STATE.lock().unwrap().wheel.is_some_and(|at| at.elapsed().as_secs_f32() < WHEEL_SECS);
    if !open {
        return;
    }
    // The Pokémon/quest panels fill about 165 units on the left, so on small
    // screens the ring slides right until its left labels clear them.
    let (rx, ry) = (60.0, 46.0);
    let cx = (b.width / 2.0).max(165.0 + rx + 26.0).min(b.width - rx - 26.0);
    let cy = b.height / 2.0;
    let n = CATALOG.emotes.len().max(1);
    for (i, e) in CATALOG.emotes.iter().enumerate() {
        let turn = std::f32::consts::TAU * i as f32 / n as f32 - std::f32::consts::FRAC_PI_2;
        let (x, y) = (cx + turn.cos() * rx, cy + turn.sin() * ry);
        let t = format!("{} {}", i + 1, e.label);
        let w = host.text_width(&t);
        panel(host, b, x - w / 2.0 - 4.0, y - 6.0, w + 8.0, 14.0, 0xD818_1A26);
        host.text(b, &t, [x - w / 2.0, y - 3.0], 0xFFFFFF, true);
    }
    let hint = "[B] close";
    let w = host.text_width(hint);
    panel(host, b, cx - w / 2.0 - 4.0, cy - 6.0, w + 8.0, 14.0, 0xA018_1A26);
    host.text(b, hint, [cx - w / 2.0, cy - 3.0], 0xC0C8D8, true);
}

fn panel(host: &mut dyn Host, b: &mut HudBatch, x: f32, y: f32, w: f32, h: f32, argb: u32) {
    host.quad(b, [[x + 1.0, y], [x + w - 1.0, y], [x + w - 1.0, y + h], [x + 1.0, y + h]], argb);
    host.quad(b, [[x, y + 1.0], [x + 1.0, y + 1.0], [x + 1.0, y + h - 1.0], [x, y + h - 1.0]], argb);
    host.quad(b, [[x + w - 1.0, y + 1.0], [x + w, y + 1.0], [x + w, y + h - 1.0], [x + w - 1.0, y + h - 1.0]], argb);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_loads_and_fits_the_number_keys() {
        assert!((6..=9).contains(&CATALOG.emotes.len()), "{}", CATALOG.emotes.len());
        let mut ids: Vec<&str> = CATALOG.emotes.iter().map(|e| e.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), CATALOG.emotes.len(), "duplicate emote id");
        for e in &CATALOG.emotes {
            assert!(!e.bubble.is_empty() && !e.label.is_empty(), "{}", e.id);
            assert!(e.looping || e.secs > 0.5, "{} needs a length", e.id);
            assert_ne!(motion(&e.motion, 0.35, e.secs.max(1.0)), Motion::default(), "{} has no fallback motion", e.id);
        }
    }

    #[test]
    fn digits_pick_from_the_wheel_in_order() {
        assert_eq!(wheel_pick("key.keyboard.1").map(|e| e.id.as_str()), Some("wave"));
        assert_eq!(wheel_pick("key.keyboard.8").map(|e| e.id.as_str()), Some("sleep"));
        assert!(wheel_pick("key.keyboard.0").is_none());
        assert!(wheel_pick("key.keyboard.9").is_none());
        assert!(wheel_pick("key.keyboard.w").is_none());
    }

    #[test]
    fn the_first_animation_the_model_has_wins() {
        let anims: Vec<String> = ["happy", "pose", "cry"].iter().map(|s| s.to_string()).collect();
        assert_eq!(pick_anim(&anims, |a| a == "cry" || a == "pose"), Some("pose"));
        assert_eq!(pick_anim(&anims, |a| a == "cry"), Some("cry"));
        assert_eq!(pick_anim(&anims, |_| false), None);
    }

    #[test]
    fn server_lines_start_and_end_emotes() {
        forget();
        assert!(heard(&["emote", "Bob", "wave"]));
        assert!(playing("Bob", false, false).is_some_and(|n| n.bubble == "Hi!"));
        assert!(heard(&["emote", "Bob", "dance"]));
        assert!(playing("Bob", false, false).is_some_and(|n| n.looping));
        assert!(heard(&["emote", "Bob", "-"]));
        assert!(playing("Bob", false, false).is_none());
        assert!(heard(&["emote", "Bob", "highfive", "Ann"]));
        assert!(playing("Bob", false, false).is_some_and(|n| n.bubble == "High five!"));
        // Made-up ids are ignored, but still an emote line.
        assert!(heard(&["emote", "Ann", "explode"]));
        assert!(playing("Ann", false, false).is_none());
        assert!(!heard(&["species", "Ann", "pikachu"]));
        forget();
    }

    #[test]
    fn motions_settle_back_to_rest() {
        for kind in ["hop", "bow", "sway", "shake", "rear", "tilt"] {
            let m = motion(kind, 2.0, 2.0);
            assert!(m.lean.abs() < 0.01 && m.roll.abs() < 0.01 && m.up.abs() < 0.01 && m.yaw.abs() < 0.01, "{kind}: {m:?}");
        }
    }
}
