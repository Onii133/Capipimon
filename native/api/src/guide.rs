//! Telling a player what they can do and what to do next: the prompt under the crosshair, the
//! low-HP pointer, the first-steps tracker and the help screen (H or /help).
//!
//! The server decides, because only it knows what F would do here; the client draws. Every key name
//! is written `[F]` in these strings and drawn as a key cap (`keyed`), so all prompts look alike.
//!
//! Wire (channel `capipimon:play`), each sent only when it changes:
//! - server -> client: `ctx\t<key|->\t<text>\t<sub>\t<0|1 warn>` / `ctx\t-`,
//!   `heal\t<P|R|B>\t<x>\t<z>` / `heal\t-`, `goal\t<progress>\t<title>\t<detail>\t<x|->\t<z|->` /
//!   `goal\t-`, `steps\t<one 0|1 per Step>\t<0|1 hidden>`.
//! - client -> server: `ui\t1` (this client draws all that: no action-bar copies), `ui\thide`.
//!
//! An older client ignores those lines and never says `ui\t1`, so it gets the same words on the
//! action bar instead.

use super::*;

// The server half: private, never published (docs/CLIENT_SERVER_SPLIT.md).
#[cfg(feature = "capipimon_server")]
include!("server/guide.rs");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Map,
    Wild,
    Heal,
    Gym,
}

pub const STEPS: [Step; 4] = [Step::Map, Step::Wild, Step::Heal, Step::Gym];

impl Step {
    fn id(self) -> &'static str {
        match self {
            Step::Map => "map",
            Step::Wild => "wild",
            Step::Heal => "heal",
            Step::Gym => "gym",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Step::Map => "Open your map",
            Step::Wild => "Beat a wild Pokémon",
            Step::Heal => "Heal up",
            Step::Gym => "Challenge your first gym",
        }
    }

    fn how(self) -> &'static str {
        match self {
            Step::Map => "Press [M]. It shows healing spots and what you've found.",
            Step::Wild => "[LMB] basic attack, [Q] [E] [R] moves.",
            Step::Heal => "Hold still in a pond or on a rest spot, or [F] at a berry bush.",
            Step::Gym => "Press [F] at a gym's door.",
        }
    }
}

fn points(c: &str) -> &'static str {
    match c {
        "P" => "Pond",
        "R" => "Rest spot",
        "B" => "Berries",
        _ => "Healing spot",
    }
}

fn how_there(c: char) -> &'static str {
    if c == 'B' { "[F] at a bush to heal" } else { "Hold still there to heal" }
}

/// The nearest of `list` to `at` (x and z only): its index and distance.
pub fn nearest(list: &[[f64; 3]], at: [f64; 3]) -> Option<(usize, f64)> {
    list.iter().enumerate().map(|(i, p)| (i, (p[0] - at[0]).hypot(p[2] - at[2]))).min_by(|a, b| a.1.total_cmp(&b.1))
}

/// Eight-point compass direction from `from` to `to` (-z is north).
pub fn compass(from: [f64; 3], to: [f64; 3]) -> &'static str {
    const NAMES: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    let deg = (to[0] - from[0]).atan2(from[2] - to[2]).to_degrees().rem_euclid(360.0);
    NAMES[(((deg + 22.5) / 45.0) as usize) % 8]
}

// ---------------------------------------------------------------- client

struct Goal {
    progress: String,
    title: String,
    detail: String,
    to: Option<[f64; 2]>,
}

#[derive(Default)]
struct View {
    ctx: Option<Ctx2>,
    heal: Option<(char, [f64; 2])>,
    goal: Option<Goal>,
    steps: [bool; 4],
    hidden: bool,
    help: bool,
}

struct Ctx2 {
    line: String,
    sub: String,
    warn: bool,
}

static VIEW: LazyLock<Mutex<View>> = LazyLock::new(Default::default);
static SAID_HELLO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The first `xp` line of a connection: tell the server this client draws the prompts.
pub fn hello(host: &mut dyn Host, f: &[&str]) {
    if f.first() == Some(&"xp") && !SAID_HELLO.swap(true, std::sync::atomic::Ordering::Relaxed) {
        let _ = host.net_send_to_server(CHANNEL, b"ui\t1");
    }
}

fn number2(x: &str, z: &str) -> Option<[f64; 2]> {
    Some([x.parse().ok()?, z.parse().ok()?])
}

pub fn heard(f: &[&str]) -> bool {
    let mut v = VIEW.lock().unwrap();
    match f {
        ["ctx", "-", ..] if f.len() == 2 => v.ctx = None,
        ["ctx", key, text, sub, warn, ..] => {
            let line = if *key == "-" { text.to_string() } else { format!("[{key}] {text}") };
            v.ctx = Some(Ctx2 { line, sub: sub.to_string(), warn: *warn == "1" });
        }
        ["heal", "-", ..] => v.heal = None,
        ["heal", k, x, z, ..] => v.heal = k.chars().next().zip(number2(x, z)),
        ["goal", "-", ..] => v.goal = None,
        ["goal", progress, title, detail, x, z, ..] => {
            v.goal = Some(Goal { progress: progress.to_string(), title: title.to_string(), detail: detail.to_string(), to: number2(x, z) });
        }
        ["steps", bits, hidden, ..] => {
            for (o, c) in v.steps.iter_mut().zip(bits.chars()) {
                *o = c == '1';
            }
            v.hidden = *hidden == "1";
        }
        _ => return false,
    }
    true
}

pub fn forget() {
    *VIEW.lock().unwrap() = View::default();
    SAID_HELLO.store(false, std::sync::atomic::Ordering::Relaxed);
}

pub fn key(host: &mut dyn Host, key: &str) -> bool {
    let mut v = VIEW.lock().unwrap();
    if key == "key.keyboard.h" {
        v.help = !v.help;
        return true;
    }
    if !v.help {
        return false;
    }
    match key {
        "key.keyboard.escape" => v.help = false,
        "key.keyboard.x" => {
            drop(v);
            let _ = host.net_send_to_server(CHANNEL, b"ui\thide");
        }
        _ => return !matches!(key, "key.keyboard.w" | "key.keyboard.a" | "key.keyboard.s" | "key.keyboard.d" | "key.keyboard.space" | "key.keyboard.left.shift"),
    }
    true
}

/// `/help` and `/controls` open the help screen. True when it was one of those.
pub fn command(command: &str) -> bool {
    if !matches!(command.trim_start_matches('/').trim(), "help" | "controls") {
        return false;
    }
    VIEW.lock().unwrap().help = true;
    true
}

/// One run of a `keyed` string: plain text, or a key name drawn as a key cap.
#[derive(Debug, PartialEq)]
enum Run<'a> {
    Text(&'a str),
    Key(&'a str),
}

fn runs(text: &str) -> Vec<Run<'_>> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let Some(len) = rest[open..].find(']') else { break };
        if open > 0 {
            out.push(Run::Text(&rest[..open]));
        }
        out.push(Run::Key(&rest[open + 1..open + len]));
        rest = &rest[open + len + 1..];
    }
    if !rest.is_empty() {
        out.push(Run::Text(rest));
    }
    out
}

const CAP_PAD: f32 = 5.0;
const CAP_FILL: u32 = 0xFFE4_E8F0;
const CAP_EDGE: u32 = 0xFF8A_94A8;
const CAP_TEXT: u32 = 0x1A20_30;
/// One line of `keyed` text, key caps included.
pub const LINE: f32 = 11.0;

/// How wide `keyed` will draw this.
pub fn keyed_width(host: &dyn Host, text: &str) -> f32 {
    runs(text).iter().map(|r| match r {
        Run::Text(t) => host.text_width(t),
        Run::Key(k) => host.text_width(k) + CAP_PAD + 1.0,
    }).sum()
}

/// Draws text where `[F]` becomes a key cap, the same everywhere in the game's own screens.
pub fn keyed(host: &mut dyn Host, b: &mut HudBatch, text: &str, at: [f32; 2], rgb: u32) {
    let mut x = at[0];
    for r in runs(text) {
        match r {
            Run::Text(t) => {
                host.text(b, t, [x, at[1]], rgb, true);
                x += host.text_width(t);
            }
            Run::Key(k) => {
                let w = host.text_width(k) + CAP_PAD;
                pill(host, b, x, at[1] - 1.0, w, 10.0, CAP_EDGE);
                pill(host, b, x + 0.5, at[1] - 1.0, w - 1.0, 9.0, CAP_FILL);
                host.text(b, k, [x + CAP_PAD / 2.0, at[1]], CAP_TEXT, false);
                x += w + 1.0;
            }
        }
    }
}

/// `keyed`, centred on `cx`.
pub fn keyed_centred(host: &mut dyn Host, b: &mut HudBatch, text: &str, cx: f32, y: f32, rgb: u32) {
    let w = keyed_width(host, text);
    keyed(host, b, text, [cx - w / 2.0, y], rgb);
}

/// Splits `text` into lines no wider than `width`, keeping key caps whole.
fn wrap(host: &dyn Host, text: &str, width: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
        if keyed_width(host, &candidate) > width && !line.is_empty() {
            lines.push(std::mem::replace(&mut line, word.to_string()));
        } else {
            line = candidate;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// How far to turn, clockwise in degrees, to face `to` (Minecraft yaw: 0 is +z, 90 is -x).
pub fn relative_deg(me: [f64; 2], yaw: f32, to: [f64; 2]) -> f32 {
    let bearing = (-(to[0] - me[0])).atan2(to[1] - me[1]).to_degrees() as f32;
    (bearing - yaw + 540.0).rem_euclid(360.0) - 180.0
}

/// A solid arrow turned `deg` clockwise from straight up.
fn arrow(host: &mut dyn Host, b: &mut HudBatch, c: [f32; 2], deg: f32, r: f32, argb: u32) {
    let at = |a: f32, l: f32| {
        let t = (deg + a).to_radians();
        [c[0] + t.sin() * l, c[1] - t.cos() * l]
    };
    let (tip, left, right, tail) = (at(0.0, r), at(-140.0, r), at(140.0, r), at(180.0, r * 0.35));
    host.quad(b, [tip, right, tail, tail], argb);
    host.quad(b, [tip, tail, left, left], argb);
}

/// The local player's place and facing, for pointing at things.
fn me(host: &dyn Host) -> Option<([f64; 2], f32)> {
    let cam = host.camera()?;
    let feet = local_feet().unwrap_or(cam.position);
    Some(([feet[0], feet[2]], cam.yaw))
}

/// Under the crosshair: what F (or standing here) does.
pub fn hud_prompt(host: &mut dyn Host, b: &mut HudBatch) {
    let v = VIEW.lock().unwrap();
    let Some(c) = &v.ctx else { return };
    let (line, sub, warn) = (c.line.clone(), c.sub.clone(), c.warn);
    drop(v);
    let (cx, y) = (b.width / 2.0, b.height / 2.0 + 26.0);
    let w = keyed_width(host, &line).max(host.text_width(&sub)) + 14.0;
    let h = if sub.is_empty() { 15.0 } else { 26.0 };
    pill(host, b, cx - w / 2.0, y - 3.0, w, h, HUD_PANEL);
    keyed_centred(host, b, &line, cx, y + 1.0, 0xFFFFFF);
    if !sub.is_empty() {
        let tw = host.text_width(&sub);
        host.text(b, &sub, [cx - tw / 2.0, y + 12.0], if warn { 0xFFD27F } else { 0xC0C8D8 }, true);
    }
}

/// Under the Pokémon card: the key strip, the low-HP pointer and the next step.
pub fn hud_panel(host: &mut dyn Host, event: &mut HudEvent<'_>) {
    const W: f32 = 160.0;
    let (heal, goal) = {
        let v = VIEW.lock().unwrap();
        (v.heal, v.goal.as_ref().map(|g| (g.progress.clone(), g.title.clone(), g.detail.clone(), g.to)))
    };
    let facing = me(&*host);
    // Always there, so nobody has to remember how to get help.
    let (mult, median) = progress::boost();
    let boosted = median > 0 && mult > 1.01;
    let strip = if boosted { 24.0 } else { 13.0 };
    let (x, y) = event.corners.place(Corner::TopLeft, W, strip);
    let b = &mut *event.batch;
    pill(host, b, x, y, W, strip, 0x9018_1A26);
    keyed(host, b, "[H] Help  [M] Map", [x + 4.0, y + 3.0], 0xE0E8F0);
    if median > 0 {
        let t = format!("Server Lv {median}");
        let tw = host.text_width(&t);
        host.text(b, &t, [x + W - 4.0 - tw, y + 3.0], 0xC0C8D8, true);
    }
    if boosted {
        host.text(b, &format!("Catching up: XP x{mult:.1}"), [x + 4.0, y + 14.0], 0x9CF27A, true);
    }
    if let Some((kind, to)) = heal {
        let (x, y) = event.corners.place(Corner::TopLeft, W, 37.0);
        let b = &mut *event.batch;
        pill(host, b, x, y, W, 37.0, HUD_PANEL);
        pill(host, b, x, y, W, 12.0, 0xFFA8_2A28);
        host.text(b, "Low HP: go heal", [x + 4.0, y + 2.0], 0xFFFFFF, true);
        let (mx, mz) = facing.map_or((to[0], to[1]), |(m, _)| (m[0], m[1]));
        if let Some((m, yaw)) = facing {
            arrow(host, b, [x + 9.0, y + 19.0], relative_deg(m, yaw, to), 5.0, 0xFFFF_D23F);
        }
        let t = format!("{}  {:.0} m", points(&kind.to_string()), (to[0] - mx).hypot(to[1] - mz));
        host.text(b, &t, [x + 18.0, y + 15.0], 0xFFFFFF, true);
        keyed(host, b, how_there(kind), [x + 4.0, y + 26.0], 0xC0C8D8);
    }
    let Some((progress, title, detail, to)) = goal else { return };
    let lines = wrap(&*host, &detail, W - 8.0);
    let h = 25.0 + lines.len() as f32 * LINE + if to.is_some() { 12.0 } else { 0.0 };
    let (x, y) = event.corners.place(Corner::TopLeft, W, h);
    let b = &mut *event.batch;
    pill(host, b, x, y, W, h, HUD_PANEL);
    pill(host, b, x, y, W, 11.0, 0xFF2A_4F8C);
    host.text(b, "NEXT", [x + 4.0, y + 1.5], 0xFFE9A0, true);
    let pw = host.text_width(&progress);
    host.text(b, &progress, [x + W - 4.0 - pw, y + 1.5], 0xFFFFFF, true);
    host.text(b, &fit(&*host, &title, W - 8.0), [x + 4.0, y + 13.0], 0xFFFFFF, true);
    for (i, l) in lines.iter().enumerate() {
        keyed(host, b, l, [x + 4.0, y + 24.0 + i as f32 * LINE], 0xC0C8D8);
    }
    if let (Some(to), Some((m, yaw))) = (to, facing) {
        let ay = y + 24.0 + lines.len() as f32 * LINE;
        arrow(host, b, [x + 9.0, ay + 5.0], relative_deg(m, yaw, to), 5.0, 0xFF8F_D3FF);
        let t = format!("{:.0} m", (to[0] - m[0]).hypot(to[1] - m[1]));
        host.text(b, &t, [x + 18.0, ay + 1.0], 0x8FD3FF, true);
    }
}

const CONTROLS: [&str; 12] = [
    "[W] [A] [S] [D]  Move",
    "[Space]  Jump",
    "[LMB]  Basic attack",
    "[Q] [E] [R]  Moves",
    "[C]  Signature move",
    "[Shift]  Guard (drains PP)",
    "Double-tap a direction: dodge",
    "[F]  Use what's in front of you",
    "[M]  Map",
    "[V]  Evolve, when offered",
    "[N]  Learn a move, when offered",
    "[T]  Chat    [B]  Emotes",
];

const HEALING: [&str; 4] = ["Hold still in a pond", "Hold still on a rest spot", "[F] at a berry bush", "All of them wait until 5 s after a fight."];
const PLACES: [&str; 4] = ["Nest: [F] PC to switch Pokémon", "Gym: [F] at the door", "Raid den: [F] to call or join", "Fainted? You hatch at a Nest."];

/// The help screen; true when it's open and drew.
pub fn overlay(host: &mut dyn Host, b: &mut HudBatch) -> bool {
    let (steps, hidden) = {
        let v = VIEW.lock().unwrap();
        if !v.help {
            return false;
        }
        (v.steps, v.hidden)
    };
    let (sw, sh) = (b.width, b.height);
    rect(host, b, 0.0, 0.0, sw, sh, 0xC010_1820);
    let (w, h) = (396.0f32.min(sw - 8.0), 214.0f32.min(sh - 8.0));
    let (x, y) = ((sw - w) / 2.0, (sh - h) / 2.0);
    pill(host, b, x, y, w, h, HUD_PANEL);
    pill(host, b, x, y, w, 14.0, 0xFF2A_4F8C);
    host.text(b, "Capipimon: how to play", [x + 6.0, y + 3.0], 0xFFFFFF, true);
    let right = x + w / 2.0 + 2.0;
    let heading = |host: &mut dyn Host, b: &mut HudBatch, t: &str, px: f32, py: f32| host.text(b, t, [px, py], 0xFFE9A0, true);
    heading(host, b, "Controls", x + 6.0, y + 19.0);
    for (i, line) in CONTROLS.iter().enumerate() {
        keyed(host, b, line, [x + 6.0, y + 32.0 + i as f32 * 13.0], 0xFFFFFF);
    }
    heading(host, b, "First steps", right, y + 19.0);
    for (i, s) in STEPS.iter().enumerate() {
        let ly = y + 31.0 + i as f32 * 11.0;
        let done = steps[i];
        rect(host, b, right, ly + 1.0, 6.0, 6.0, if done { 0xFF4C_D964 } else { 0xFF3A_3D4D });
        host.text(b, s.title(), [right + 10.0, ly], if done { 0x9CF27A } else { 0xFFFFFF }, true);
    }
    heading(host, b, "Healing", right, y + 79.0);
    for (i, line) in HEALING.iter().enumerate() {
        keyed(host, b, line, [right, y + 91.0 + i as f32 * LINE], if i == 3 { 0xC0C8D8 } else { 0xFFFFFF });
    }
    heading(host, b, "Places", right, y + 139.0);
    for (i, line) in PLACES.iter().enumerate() {
        keyed(host, b, line, [right, y + 151.0 + i as f32 * LINE], 0xFFFFFF);
    }
    let foot = if hidden { "[H] close  [X] show the First steps tracker again" } else { "[H] close  [X] hide the First steps tracker" };
    keyed_centred(host, b, foot, x + w / 2.0, y + h - 11.0, 0xD8E4F4);
    true
}

